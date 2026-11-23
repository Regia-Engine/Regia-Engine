// SPDX-License-Identifier: MIT-0
//! The app: the shared player (`regia_web::Player`, the web player's core) on the platform layer. Input is
//! sorted into the panel or the view, the player's lazy loop decides whether anything draws, and the
//! lifecycle drops and recreates the surface. There is no engine renderer here.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use egui::{Event, Modifiers, PointerButton, Pos2, RawInput, Rect, Vec2};
use regia_app::prelude::*;
use regia_app::{Lifecycle, LoopControl, LoopMode};
use regia_platform::{
    AppLifecycle, BackPressed, ButtonState, ClosePolicy, CursorMoved, DisplayHandle, InputTimestamp,
    LifecycleEvent, MouseButton, MouseButtonInput, MouseWheel, PrimaryWindow, SafeArea, TouchInput,
    TouchPhase, Window, WindowHandles,
};
use regia_viewer::BakedEnv;
use regia_viewer_pack::Package;
use regia_web::player::{Player, PlayerEvent};
use regia_web::product::PackageProduct;
use regia_web::scene::{ViewerScene, player_config};
use regia_web::seams::PointerClass;
use regia_web::view::ProductView;
use regia_web::{Schedule, WebError};
use serde_json::{Value, json};

use crate::fetch::{HttpsSource, PackageUrl};
use crate::panel::{self, PanelAction, PanelGroup, PanelModel};
use crate::pipeline_cache::PipelineCacheFile;
use crate::stats::{ViewerStats, wakes_per_second};
use crate::store::{PackageEntry, PackageStore, StoreError};
use crate::surface::{BackendChoice, Capture, GpuError, SurfaceHost, SurfaceTarget};
use crate::view::ShiftedView;

/// The finger id desktop mouse input uses.
const MOUSE_FINGER: u64 = u64::MAX;
/// A pointer that lands here belongs to the panel, this many points outside its rectangle.
const PANEL_MARGIN: f32 = 2.0;
/// The studio the product's build names.
const STUDIO_ID: &str = "studio-default";

/// Where packages live and what to do at start.
#[derive(Resource, Debug, Clone, Default)]
pub struct ViewerConfig {
    /// Package folders, first one takes downloads.
    pub package_roots: Vec<PathBuf>,
    /// Download this package folder (`https://...`) at start.
    pub fetch_url: Option<String>,
    /// Open this package at start.
    pub open_package: Option<String>,
    /// The place to tell people to copy packages to.
    pub push_hint: String,
    /// The graphics API to ask for.
    pub backend: BackendChoice,
    /// MSAA samples, 1 or 4, when a test run forces them. `None`: what the adapter supports.
    pub samples: Option<u32>,
    /// Where compiled pipelines are kept between runs. `None`: compile them at every start.
    pub pipeline_cache_dir: Option<PathBuf>,
}

/// `1` or `4`; `None` for anything else.
#[must_use]
pub fn parse_samples(name: &str) -> Option<u32> {
    match name {
        "1" => Some(1),
        "4" => Some(4),
        _ => None,
    }
}

/// Messages of a background package task.
enum Task {
    Progress(u64, u64),
    Installed(Result<String, StoreError>),
    Opened(String, Instant, Result<Box<Package>, StoreError>),
}

/// A finger's owner for the length of a gesture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Panel,
    View,
}

/// One pointer event as the app routes it: position in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Touch {
    /// Pointer id.
    pub id: u64,
    /// What happened.
    pub phase: TouchPhase,
    /// A finger or a mouse.
    pub class: PointerClass,
    /// Position from the top-left, physical pixels.
    pub x: f32,
    /// Position from the top-left, physical pixels.
    pub y: f32,
    /// When the event reached the engine.
    pub at: Duration,
}

/// The shared player and what it draws through. Not `Send` (the player's seams are not), so it lives on the
/// app's main thread as a non-send resource.
pub struct ViewerCore {
    /// The player.
    pub player: Player,
    /// The window surface and the panel's picture, once a window exists.
    pub host: Option<Rc<RefCell<SurfaceHost>>>,
    /// Where the panel pushes the piece on screen.
    shift: Rc<Cell<[f32; 2]>>,
    /// When the process's first frame started counting.
    started: Instant,
    /// The frame a test asked for with [`SurfaceHost::capture_next_frame`].
    pub last_capture: Option<Capture>,
    /// When the pick on its way to the screen was applied.
    switch_started: Option<Instant>,
    /// MSAA samples a test run forces.
    pub samples: Option<u32>,
    /// Where compiled pipelines are kept between runs.
    pub pipeline_cache_dir: Option<PathBuf>,
    /// What the last [`Self::attach`] did: `viewer_app.stats` reports it as `gpu_start`.
    pub gpu_start: Option<Value>,
}

impl ViewerCore {
    /// A core with no window yet.
    #[must_use]
    pub fn new() -> Self {
        let epoch = Instant::now();
        Self {
            player: Player::new(Box::new(move || epoch.elapsed().as_secs_f64() * 1000.0)),
            host: None,
            shift: Rc::new(Cell::new([0.0, 0.0])),
            started: epoch,
            last_capture: None,
            switch_started: None,
            samples: None,
            pipeline_cache_dir: None,
            gpu_start: None,
        }
    }

    /// Attaches the renderer to a host: every pipeline is built here, once per process.
    pub fn attach(&mut self, host: SurfaceHost) -> Result<(), WebError> {
        let began = Instant::now();
        let (device, queue, format, samples) = (
            host.device().clone(),
            host.queue().clone(),
            host.format(),
            self.samples
                .unwrap_or_else(|| regia_viewer::gpu::sample_count_for(host.adapter(), host.format())),
        );
        let cache_file = self
            .pipeline_cache_dir
            .as_deref()
            .and_then(|dir| PipelineCacheFile::open(dir, host.adapter(), host.device()));
        let host = Rc::new(RefCell::new(host));
        let mut config = player_config(format, samples);
        config.pipeline_cache = cache_file.as_ref().map(|file| file.cache().clone());
        let scene = ViewerScene::new(
            device,
            queue,
            config,
            &BakedEnv::studio_default(),
            SurfaceTarget(host.clone()),
        )?;
        self.player.attach_scene(Box::new(scene));
        self.host = Some(host);
        let pipelines_ms = began.elapsed().as_secs_f64() * 1000.0;
        let (loaded, saved) = cache_file.as_ref().map_or((0, 0), |file| {
            let saved = file.save().unwrap_or_else(|err| {
                tracing::warn!(target: "product_viewer", "pipeline cache not saved: {err}");
                0
            });
            (file.loaded_bytes(), saved)
        });
        tracing::info!(
            target: "product_viewer",
            "viewer_pipelines_ms {pipelines_ms:.1} samples {samples} cache_loaded {loaded} cache_saved {saved}"
        );
        self.gpu_start = Some(json!({
            "pipelines_ms": pipelines_ms,
            "samples": samples,
            "pipeline_cache_loaded_bytes": loaded,
            "pipeline_cache_saved_bytes": saved,
        }));
        Ok(())
    }

    /// Shows a package: the product, its view inside the limits its recipe gives, its first frame.
    pub fn open(&mut self, package: Package) -> Result<Value, WebError> {
        let mut product = PackageProduct::open(package, STUDIO_ID)?;
        let view = ProductView::for_product(&mut product)?;
        self.player.install(
            Box::new(product),
            Box::new(ShiftedView::new(view, self.shift.clone())),
        )?;
        self.player.options()
    }
}

impl Default for ViewerCore {
    fn default() -> Self {
        Self::new()
    }
}

/// Everything the app holds outside the player.
#[derive(Resource)]
pub struct ViewerState {
    /// The open package's folder name.
    pub open_slug: Option<String>,
    /// The title over the options.
    pub title: String,
    /// The options as of the current picks.
    pub groups: Vec<PanelGroup>,
    /// The picks that still wait for the player, from the panel or the agent.
    pending_picks: Vec<(String, Value)>,
    /// Counters and timings.
    pub stats: ViewerStats,
    /// The status line.
    pub status: String,
    /// The status line is a refusal or an error.
    pub status_is_error: bool,
    /// The product list is showing.
    pub showing_products: bool,
    /// Packages on the device.
    pub packages: Vec<PackageEntry>,
    /// The store.
    pub store: PackageStore,
    push_hint: String,
    routes: BTreeMap<u64, Route>,
    mouse_down: bool,
    mouse_at: (f32, f32),
    task: Option<Mutex<Receiver<Task>>>,
    /// Pointer input for the panel, not yet given to egui.
    panel_events: Vec<Event>,
    /// Pointer input for the view, not yet given to the player.
    view_events: Vec<Touch>,
    /// Wheel lines for the view.
    wheel_lines: f32,
    /// What the panel was last laid out for: window size, scale, safe area.
    layout: Option<((u32, u32), u32, SafeArea)>,
    /// The next frame must draw (a new surface, a lifecycle change).
    redraw: bool,
}

impl ViewerState {
    /// A fresh state over `config`.
    #[must_use]
    pub fn new(config: &ViewerConfig) -> Self {
        let store = PackageStore::new(config.package_roots.clone());
        let packages = store.list();
        Self {
            open_slug: None,
            title: "Products".to_owned(),
            groups: Vec::new(),
            pending_picks: Vec::new(),
            stats: ViewerStats::default(),
            status: String::new(),
            status_is_error: false,
            showing_products: true,
            packages,
            store,
            push_hint: config.push_hint.clone(),
            routes: BTreeMap::new(),
            mouse_down: false,
            mouse_at: (0.0, 0.0),
            task: None,
            panel_events: Vec::new(),
            view_events: Vec::new(),
            wheel_lines: 0.0,
            layout: None,
            redraw: true,
        }
    }

    fn set_status(&mut self, status: String, is_error: bool) {
        self.status = status;
        self.status_is_error = is_error;
    }

    fn busy_error(what: &str) -> StoreError {
        StoreError::new("package_busy", format!("{what}: another package task is running"))
    }

    /// Downloads a package folder in the background; the app stays responsive, and works offline once it
    /// landed. Refuses when a package task is already running.
    pub fn start_fetch(&mut self, url: &str) -> Result<(), StoreError> {
        if self.task.is_some() {
            return Err(Self::busy_error(url));
        }
        let parsed = PackageUrl::parse(url)?;
        let store = self.store.clone();
        let (send, receive) = mpsc::channel();
        std::thread::Builder::new()
            .name("package-fetch".to_owned())
            .spawn(move || {
                let progress_send = send.clone();
                let result = HttpsSource::new(parsed).and_then(|mut source| {
                    store.install(&mut source, &mut |done, total| {
                        let _ = progress_send.send(Task::Progress(done, total));
                    })
                });
                let _ = send.send(Task::Installed(result));
            })
            .map_err(|err| StoreError::new("package_store_failed", err.to_string()))?;
        self.task = Some(Mutex::new(receive));
        self.set_status("Downloading package".to_owned(), false);
        self.redraw = true;
        Ok(())
    }

    /// Reads and verifies a package in the background, off the frame thread.
    pub fn start_open(&mut self, slug: &str) -> Result<(), StoreError> {
        if self.task.is_some() {
            return Err(Self::busy_error(slug));
        }
        let store = self.store.clone();
        let slug = slug.to_owned();
        let (send, receive) = mpsc::channel();
        let started = Instant::now();
        std::thread::Builder::new()
            .name("package-open".to_owned())
            .spawn(move || {
                let opened = store.open(&slug).map(Box::new);
                let _ = send.send(Task::Opened(slug, started, opened));
            })
            .map_err(|err| StoreError::new("package_store_failed", err.to_string()))?;
        self.task = Some(Mutex::new(receive));
        Ok(())
    }

    /// Whether a package task runs.
    #[must_use]
    pub fn task_running(&self) -> bool {
        self.task.is_some()
    }

    /// The player's picks changed outside the panel (an agent call): show them next frame.
    pub(crate) fn note_external_change(&mut self, core: &mut ViewerCore) {
        if let Ok(options) = core.player.options() {
            self.groups = panel::panel_groups(&options);
        }
        self.redraw = true;
    }

    /// Queues a pick for the next frame.
    pub fn queue_pick(&mut self, group: String, value: Value) {
        self.pending_picks.push((group, value));
    }

    fn report_error(&mut self, code: &str, message: &str) {
        tracing::warn!(target: "product_viewer", "{code}: {message}");
        self.set_status(format!("{code}: {message}"), true);
        self.redraw = true;
    }

    /// Takes what the package task reports, if anything: a package ready to show comes back.
    fn poll_task(&mut self) -> Option<(String, Instant, Box<Package>)> {
        let task = self.task.as_mut().and_then(|task| task.get_mut().ok())?;
        let mut finished = None;
        let mut progress = None;
        loop {
            match task.try_recv() {
                Ok(Task::Progress(done, total)) => progress = Some((done, total)),
                Ok(message @ (Task::Installed(_) | Task::Opened(..))) => {
                    finished = Some(message);
                    break;
                }
                Err(TryRecvError::Empty) => {
                    if let Some((done, total)) = progress {
                        self.set_status(
                            format!("Downloading package: {} of {} KB", done / 1024, total / 1024),
                            false,
                        );
                        self.redraw = true;
                    }
                    return None;
                }
                Err(TryRecvError::Disconnected) => break,
            }
        }
        self.task = None;
        self.redraw = true;
        self.packages = self.store.list();
        match finished {
            Some(Task::Installed(Ok(slug))) => {
                tracing::info!(target: "product_viewer", "package {slug} installed");
                self.set_status(format!("Package {slug} installed"), false);
                if let Err(err) = self.start_open(&slug) {
                    self.report_error(err.code, &err.message);
                }
                None
            }
            Some(Task::Installed(Err(err)) | Task::Opened(_, _, Err(err))) => {
                self.report_error(err.code, &err.message);
                self.showing_products = true;
                None
            }
            Some(Task::Opened(slug, started, Ok(package))) => Some((slug, started, package)),
            _ => None,
        }
    }

    /// One finger event. The first finger of a gesture decides who owns it: the panel or the view.
    pub(crate) fn touch(&mut self, event: Touch, panel_px: Rect, scale: f32) {
        let route = match event.phase {
            TouchPhase::Started => {
                let route = self.routes.values().next().copied().unwrap_or_else(|| {
                    if panel_px.contains(Pos2::new(event.x, event.y)) {
                        Route::Panel
                    } else {
                        Route::View
                    }
                });
                self.routes.insert(event.id, route);
                route
            }
            TouchPhase::Moved => match self.routes.get(&event.id) {
                Some(route) => *route,
                None => return,
            },
            TouchPhase::Ended | TouchPhase::Cancelled => match self.routes.remove(&event.id) {
                Some(route) => route,
                None => return,
            },
        };
        match route {
            Route::Panel => self.panel_pointer(&event, scale),
            Route::View => self.view_events.push(event),
        }
    }

    fn panel_pointer(&mut self, event: &Touch, scale: f32) {
        let at = Pos2::new(event.x / scale, event.y / scale);
        let button = |pressed| Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        match event.phase {
            TouchPhase::Started => {
                self.panel_events.push(Event::PointerMoved(at));
                self.panel_events.push(button(true));
            }
            TouchPhase::Moved => self.panel_events.push(Event::PointerMoved(at)),
            TouchPhase::Ended => {
                self.panel_events.push(button(false));
                self.panel_events.push(Event::PointerGone);
            }
            TouchPhase::Cancelled => self.panel_events.push(Event::PointerGone),
        }
    }

    fn apply_actions(&mut self, actions: Vec<PanelAction>) {
        for action in actions {
            match action {
                PanelAction::Pick { group, value } => {
                    let wanted = self
                        .groups
                        .iter()
                        .find(|g| g.group == group)
                        .map_or_else(|| json!(value), |g| panel::pick_value(g, &value));
                    self.queue_pick(group, wanted);
                }
                PanelAction::ShowProducts => {
                    self.packages = self.store.list();
                    self.showing_products = true;
                    self.redraw = true;
                }
                PanelAction::HideProducts => {
                    self.showing_products = false;
                    self.redraw = true;
                }
                PanelAction::Open(slug) => {
                    if let Err(err) = self.start_open(&slug) {
                        self.report_error(err.code, &err.message);
                    }
                }
            }
        }
    }

    /// The status line after an accepted change: what the build is, and a plain note on a partial listing.
    fn show_build(&mut self, build: &Value, incomplete: bool) {
        let statement = build
            .pointer("/resolution/statement")
            .and_then(Value::as_str)
            .unwrap_or("");
        let status = if incomplete {
            format!("Partial listing. {statement}")
        } else {
            statement.to_owned()
        };
        self.set_status(status, false);
    }
}

/// egui and the panel's run state.
#[derive(Resource, Default)]
pub struct PanelUi {
    ctx: egui::Context,
    /// Scripted capture: the time the panel sees, from the script's clock. Unset: the wall clock.
    fixed_time: Option<f64>,
    /// The panel's rectangle, points.
    rect: Option<Rect>,
    /// The panel must be run again (its model changed, or it asked for a repaint).
    stale: bool,
    started: Option<Instant>,
    configured: bool,
}

impl PanelUi {
    /// The panel's rectangle in physical pixels, with the routing margin; empty before the first run.
    #[must_use]
    pub fn hit_rect_px(&self, scale: f32) -> Rect {
        self.rect.map_or(Rect::NOTHING, |r| {
            let grown = r.expand(PANEL_MARGIN);
            Rect::from_min_max(grown.min * scale, grown.max * scale)
        })
    }

    /// Runs the UI, hands its picture to the host, returns what was tapped.
    fn run(
        &mut self,
        state: &mut ViewerState,
        host: Option<&Rc<RefCell<SurfaceHost>>>,
        size_px: (u32, u32),
        scale: f32,
        safe: &SafeArea,
    ) -> Vec<PanelAction> {
        let _span = tracing::info_span!("viewer_panel").entered();
        if !self.configured {
            panel::configure(&self.ctx);
            self.configured = true;
        }
        let points = |px: u32| px as f32 / scale;
        let size = Vec2::new(points(size_px.0), points(size_px.1));
        let now = match self.fixed_time {
            Some(time) => time,
            None => self
                .started
                .get_or_insert_with(Instant::now)
                .elapsed()
                .as_secs_f64(),
        };
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
            events: std::mem::take(&mut state.panel_events),
            time: Some(now),
            ..RawInput::default()
        };
        let model = PanelModel {
            title: &state.title,
            groups: &state.groups,
            status: &state.status,
            status_is_error: state.status_is_error,
            showing_products: state.showing_products || state.open_slug.is_none(),
            product_open: state.open_slug.is_some(),
            products: &state.packages,
            push_hint: &state.push_hint,
            safe: [
                points(safe.left),
                points(safe.top),
                points(safe.right),
                points(safe.bottom),
            ],
        };
        let mut actions = Vec::new();
        let mut rect = None;
        self.ctx.set_pixels_per_point(scale);
        let mut output = self.ctx.run_ui(input, |ui| {
            let (tapped, panel_rect) = panel::show(ui, &model);
            actions = tapped;
            rect = Some(panel_rect);
        });
        self.rect = rect;
        let primitives = self
            .ctx
            .tessellate(std::mem::take(&mut output.shapes), output.pixels_per_point);
        let textures = std::mem::take(&mut output.textures_delta);
        match host {
            Some(host) => host.borrow_mut().set_panel(primitives, textures, scale),
            None => {
                // No device yet: run again when there is one (`resume` marks it stale).
                let mut textures = textures;
                textures.clear();
                self.configured = false;
                self.ctx = egui::Context::default();
            }
        }
        // The panel animates something (a press highlight, scroll momentum): another run soon.
        self.stale = output
            .viewport_output
            .values()
            .any(|viewport| viewport.repaint_delay < Duration::from_millis(100));
        actions
    }
}

/// Installs the viewer: resources, input, the frame system, the surface lifecycle.
pub struct ProductViewerPlugin {
    /// Where packages live and what to do at start.
    pub config: ViewerConfig,
}

impl Plugin for ProductViewerPlugin {
    fn build(&self, app: &mut App) {
        let state = ViewerState::new(&self.config);
        let mut control = LoopControl::default();
        control.set_mode(LoopMode::Lazy);
        control.throttled_fps = wakes_per_second(true, false);
        app.insert_resource(control)
            .insert_resource(self.config.clone())
            .insert_resource(state)
            .init_resource::<PanelUi>()
            // Back goes from the product to the list; the list's Back leaves the app.
            .insert_resource(ClosePolicy {
                exit_on_primary_close: true,
                back_closes_primary: false,
            })
            .add_systems(Startup, start_tasks)
            .add_systems(Lifecycle, surface_lifecycle)
            .add_systems(Update, (read_input, draw_frame).chain());
        let mut core = ViewerCore::new();
        core.samples = self.config.samples;
        core.pipeline_cache_dir
            .clone_from(&self.config.pipeline_cache_dir);
        app.world_mut().insert_non_send(core);
    }
}

fn start_tasks(config: Res<ViewerConfig>, mut state: ResMut<ViewerState>) {
    let started = if let Some(url) = &config.fetch_url {
        state.start_fetch(url)
    } else {
        let wanted = config.open_package.clone().or_else(|| {
            state
                .packages
                .iter()
                .find(|p| p.problem.is_none())
                .map(|p| p.slug.clone())
        });
        wanted.map_or(Ok(()), |slug| state.start_open(&slug))
    };
    if let Err(err) = started {
        state.report_error(err.code, &err.message);
    }
}

/// `Lifecycle`: the surface must be gone before the suspend returns, and comes back on resume on the same
/// device. The product and every GPU buffer stay.
fn surface_lifecycle(world: &mut World) {
    let Some(event) = world.resource::<AppLifecycle>().last_event else {
        return;
    };
    match event {
        LifecycleEvent::Suspended | LifecycleEvent::WillTerminate => {
            let fingers: Vec<u64> = {
                let mut state = world.resource_mut::<ViewerState>();
                let fingers = state
                    .routes
                    .iter()
                    .filter(|(_, route)| **route == Route::View)
                    .map(|(id, _)| *id)
                    .collect();
                state.routes.clear();
                state.panel_events.push(Event::PointerGone);
                state.view_events.clear();
                state.stats.note_suspend();
                fingers
            };
            let Some(mut core) = world.get_non_send_mut::<ViewerCore>() else {
                return;
            };
            for finger in fingers {
                let _ = core.player.pointer_cancel(finger);
            }
            if let Some(host) = &core.host {
                host.borrow_mut().drop_surface();
            }
        }
        LifecycleEvent::Resumed => resume(world),
        LifecycleEvent::Started | LifecycleEvent::LowMemory => {}
    }
}

fn resume(world: &mut World) {
    let window = world
        .resource::<WindowHandles>()
        .iter()
        .next()
        .map(|(entity, os)| (entity, os.clone()));
    let Some((entity, os)) = window else {
        tracing::error!(target: "product_viewer", "resumed without a window");
        return;
    };
    let size = world.get::<Window>(entity).map_or((0, 0), |w| w.physical_size);
    let size = if size.0 == 0 || size.1 == 0 {
        let inner = os.inner_size();
        (inner.width, inner.height)
    } else {
        size
    };
    let display = world.get_resource::<DisplayHandle>().cloned();
    let backend = world
        .get_resource::<ViewerConfig>()
        .map_or_else(BackendChoice::default, |config| config.backend);
    let result = match world.get_non_send_mut::<ViewerCore>() {
        Some(mut core) => match (core.host.clone(), display) {
            (Some(host), _) => host
                .borrow_mut()
                .recreate_surface(&os, size)
                .map_err(|err: GpuError| err.to_string()),
            (None, Some(display)) => SurfaceHost::new(&os, display, size, backend)
                .map_err(|err| err.to_string())
                .and_then(|host| core.attach(host).map_err(|err| err.to_string())),
            (None, None) => Err("no display handle".to_owned()),
        },
        None => return,
    };
    let mut state = world.resource_mut::<ViewerState>();
    if let Err(err) = result {
        tracing::error!(target: "product_viewer", "no GPU: {err}");
        state.set_status(format!("gpu_unavailable: {err}"), true);
        return;
    }
    state.stats.note_resume();
    state.redraw = true;
    world.resource_mut::<PanelUi>().stale = true;
}

fn stamp(stamp: InputTimestamp) -> Duration {
    stamp.since_epoch()
}

/// `Update`: sorts raw input into the panel and the view.
#[allow(clippy::too_many_arguments)] // One reader per raw input message.
fn read_input(
    mut touches: MessageReader<TouchInput>,
    mut buttons: MessageReader<MouseButtonInput>,
    mut cursors: MessageReader<CursorMoved>,
    mut wheels: MessageReader<MouseWheel>,
    mut back: MessageReader<BackPressed>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut state: ResMut<ViewerState>,
    panel_ui: Res<PanelUi>,
    mut exit: MessageWriter<AppExit>,
) {
    let Ok(window) = windows.single() else {
        return;
    };
    let scale = window.scale_factor.max(0.5) as f32;
    let panel = panel_ui.hit_rect_px(scale);
    for event in touches.read() {
        let touch = Touch {
            id: event.id,
            phase: event.phase,
            class: PointerClass::Touch,
            x: event.position.0 as f32,
            y: event.position.1 as f32,
            at: stamp(event.timestamp),
        };
        state.touch(touch, panel, scale);
    }
    for event in cursors.read() {
        let at = (event.position.0 as f32, event.position.1 as f32);
        state.mouse_at = at;
        if state.mouse_down {
            let touch = Touch {
                id: MOUSE_FINGER,
                phase: TouchPhase::Moved,
                class: PointerClass::Mouse,
                x: at.0,
                y: at.1,
                at: stamp(event.timestamp),
            };
            state.touch(touch, panel, scale);
        }
    }
    for event in buttons.read() {
        if event.button != MouseButton::Left {
            continue;
        }
        let pressed = event.state == ButtonState::Pressed;
        state.mouse_down = pressed;
        let (x, y) = state.mouse_at;
        let touch = Touch {
            id: MOUSE_FINGER,
            phase: if pressed {
                TouchPhase::Started
            } else {
                TouchPhase::Ended
            },
            class: PointerClass::Mouse,
            x,
            y,
            at: stamp(event.timestamp),
        };
        state.touch(touch, panel, scale);
    }
    for event in wheels.read() {
        state.wheel_lines += event.delta.1;
    }
    for _ in back.read() {
        if state.showing_products || state.open_slug.is_none() {
            exit.write(AppExit::Success);
        } else {
            state.packages = state.store.list();
            state.showing_products = true;
            state.redraw = true;
        }
    }
}

/// The primary window's scale factor.
#[must_use]
pub fn primary_window_scale(world: &World) -> f32 {
    world
        .resource::<WindowHandles>()
        .iter()
        .next()
        .and_then(|(entity, _)| world.get::<Window>(entity))
        .map_or(1.0, |w| w.scale_factor.max(0.5) as f32)
}

/// How far to move the piece on screen (fractions of the window, +x right, +y up) to center it in what the
/// panel leaves free.
#[must_use]
pub fn piece_shift(window_points: Vec2, panel: Option<Rect>) -> [f32; 2] {
    let Some(panel) = panel else {
        return [0.0, 0.0];
    };
    if window_points.x <= 0.0 || window_points.y <= 0.0 {
        return [0.0, 0.0];
    }
    if panel.min.y > 1.0 {
        [0.0, panel.height() * 0.5 / window_points.y]
    } else {
        [-panel.width() * 0.5 / window_points.x, 0.0]
    }
}

/// Feeds queued view input to the player. Returns whether a frame was asked for.
fn feed_view(state: &mut ViewerState, core: &mut ViewerCore) -> bool {
    let mut asked = false;
    for event in std::mem::take(&mut state.view_events) {
        let t_ms = event.at.as_secs_f64() * 1000.0;
        let schedule = match event.phase {
            TouchPhase::Started => core
                .player
                .pointer_down(event.id, event.class, event.x, event.y, t_ms),
            TouchPhase::Moved => core.player.pointer_move(event.id, event.x, event.y, t_ms),
            TouchPhase::Ended => core.player.pointer_up(event.id, t_ms),
            TouchPhase::Cancelled => core.player.pointer_cancel(event.id),
        };
        asked |= schedule == Schedule::Request;
    }
    let lines = std::mem::take(&mut state.wheel_lines);
    if lines != 0.0 {
        asked |= core.player.wheel(lines) == Schedule::Request;
    }
    asked
}

/// Applies queued picks. A refusal changes nothing and is said on the status line.
fn apply_picks(state: &mut ViewerState, core: &mut ViewerCore) {
    for (group, value) in std::mem::take(&mut state.pending_picks) {
        let started = Instant::now();
        match core.player.set_pick(&group, &value, false) {
            Ok((result, _)) => {
                core.switch_started = Some(started);
                let incomplete = result.build.pointer("/resolution/reasons").is_some_and(|r| {
                    r.as_array()
                        .is_some_and(|reasons| reasons.iter().any(|x| x == "incomplete"))
                });
                state.show_build(&result.build, incomplete);
                if let Ok(options) = core.player.options() {
                    state.groups = panel::panel_groups(&options);
                }
            }
            Err(err) => {
                state.set_status(err.message.clone(), true);
                tracing::info!(target: "product_viewer", "pick refused: {err}");
            }
        }
        state.redraw = true;
    }
}

/// Takes a package the background task finished: shows it, or says why not.
fn install_finished_package(state: &mut ViewerState, core: &mut ViewerCore) {
    let Some((slug, started, package)) = state.poll_task() else {
        return;
    };
    let title = package
        .options
        .pointer("/option_graph/title")
        .and_then(Value::as_str)
        .map_or_else(|| slug.clone(), str::to_owned);
    match core.open(*package) {
        Ok(options) => {
            let open_ms = started.elapsed().as_secs_f32() * 1000.0;
            tracing::info!(target: "product_viewer", "viewer_package_open_ms {open_ms:.2} {slug}");
            state.stats.package_open_ms = Some(open_ms);
            state.groups = panel::panel_groups(&options);
            state.title = title;
            state.open_slug = Some(slug);
            state.showing_products = false;
            if let Ok(build) = core.player.build() {
                state.show_build(&build, false);
            }
        }
        Err(err) => {
            state.report_error(err.code, &err.message);
            state.showing_products = true;
        }
    }
    state.redraw = true;
}

/// Drains the player's events into the status line and the log.
fn drain_events(state: &mut ViewerState, core: &mut ViewerCore) {
    for event in core.player.drain_events() {
        match event {
            PlayerEvent::Ready { incomplete } => {
                if incomplete && let Ok(build) = core.player.build() {
                    state.show_build(&build, true);
                    state.redraw = true;
                }
                if state.stats.first_frame_ms.is_none() {
                    let first = core.started.elapsed().as_secs_f32() * 1000.0;
                    tracing::info!(target: "product_viewer", "viewer_first_frame_ms {first:.2}");
                    state.stats.first_frame_ms = Some(first);
                }
            }
            PlayerEvent::Changed { .. } => {}
            PlayerEvent::Error(err) => {
                // A refused pick is already on the status line, in plainer words.
                if err.code != "pick_not_sold" {
                    state.report_error(err.code, &err.message);
                }
            }
        }
    }
}

/// `Update`: the player's frame, when anything is due. Exclusive: the player lives on this thread.
fn draw_frame(world: &mut World) {
    let window = world
        .resource::<WindowHandles>()
        .iter()
        .next()
        .map(|(entity, _)| entity);
    let Some(entity) = window else {
        return;
    };
    let (size, scale) = world.get::<Window>(entity).map_or(((0, 0), 1.0), |w| {
        (w.physical_size, w.scale_factor.max(0.5) as f32)
    });
    let safe = *world.resource::<SafeArea>();
    let Some(mut core) = world.remove_non_send::<ViewerCore>() else {
        return;
    };
    world.resource_scope(|world, mut state: Mut<ViewerState>| {
        world.resource_scope(|_world, mut panel_ui: Mut<PanelUi>| {
            frame(&mut state, &mut panel_ui, &mut core, size, scale, &safe);
        });
    });
    let wakes = world.resource::<ViewerState>().stats.wakes_per_second;
    let mut control = world.resource_mut::<LoopControl>();
    if (control.throttled_fps - wakes).abs() > f32::EPSILON {
        control.throttled_fps = wakes;
    }
    world.insert_non_send(core);
}

/// Takes what waits for the next picture: a package, the layout, the view's input, the panel's taps, the picks.
fn sync_input_and_panel(
    state: &mut ViewerState,
    panel_ui: &mut PanelUi,
    core: &mut ViewerCore,
    host: Option<&Rc<RefCell<SurfaceHost>>>,
    size: (u32, u32),
    scale: f32,
    safe: &SafeArea,
) {
    install_finished_package(state, core);
    // A rotation or a new inset: lay out again and draw.
    let layout = (size, scale.to_bits(), *safe);
    if state.layout != Some(layout) {
        state.layout = Some(layout);
        panel_ui.stale = true;
        let css = [size.0 as f32 / scale, size.1 as f32 / scale];
        let _ = core.player.resize(css, [size.0.max(1), size.1.max(1)]);
    }
    let mut panel_dirty = panel_ui.stale || !state.panel_events.is_empty() || state.redraw;
    feed_view(state, core);
    if panel_dirty {
        let actions = panel_ui.run(state, host, size, scale, safe);
        state.apply_actions(actions);
    }
    let had_picks = !state.pending_picks.is_empty();
    apply_picks(state, core);
    drain_events(state, core);
    // A tap changes what the panel shows: show it in the frame that shows the pick.
    if had_picks || state.redraw {
        let actions = panel_ui.run(state, host, size, scale, safe);
        state.apply_actions(actions);
        panel_dirty = true;
    }
    state.redraw = false;
    if panel_dirty {
        let _ = core.player.invalidate();
    }
}

/// What one scripted step did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedStep {
    /// A frame was drawn.
    pub drew: bool,
    /// The frame read back, when one was asked for and drawn.
    pub capture: Option<Capture>,
}

/// What a scripted step is asked to do.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StepRequest {
    /// The frame's time, ms.
    pub dt_ms: f64,
    /// Draw even when nothing changed.
    pub force: bool,
    /// Read the frame back, panel included, as the screen shows it.
    pub capture: bool,
}

/// One scripted frame (the agent's `viewer_app.step`): the panel and the picture on the player's fixed clock, nothing
/// on the wall clock.
///
/// # Errors
/// The player's refusal: not in deterministic mode, or a bad time.
pub fn scripted_step(
    state: &mut ViewerState,
    panel_ui: &mut PanelUi,
    core: &mut ViewerCore,
    size: (u32, u32),
    scale: f32,
    safe: &SafeArea,
    request: StepRequest,
) -> Result<ScriptedStep, WebError> {
    let StepRequest {
        dt_ms,
        force,
        capture,
    } = request;
    let _span = tracing::info_span!("viewer_scripted_step").entered();
    if !core.player.lazy().manual() {
        return Err(WebError::new("bad_argument", "step needs deterministic mode"));
    }
    let host = core.host.clone();
    let (_, clock_ms) = core.player.script_clock();
    panel_ui.fixed_time = Some((clock_ms + dt_ms.max(0.0)) / 1000.0);
    sync_input_and_panel(state, panel_ui, core, host.as_ref(), size, scale, safe);
    core.shift.set(piece_shift(
        Vec2::new(size.0 as f32 / scale, size.1 as f32 / scale),
        panel_ui.rect,
    ));
    if capture && let Some(host) = &host {
        host.borrow_mut().capture_next_frame();
    }
    let stepped = if force {
        core.player.step_forced(dt_ms)
    } else {
        core.player.step(dt_ms)
    };
    let mut read = None;
    if let Some(host) = &host {
        let mut host = host.borrow_mut();
        host.cancel_capture();
        // A host with no window reads back every frame; only what was asked for is handed on.
        read = host.take_capture().filter(|_| capture);
    }
    drain_events(state, core);
    let drew = stepped?;
    Ok(ScriptedStep { drew, capture: read })
}

/// Leaves scripted capture: the panel goes back to the wall clock and the lazy loop takes over.
pub fn end_scripted(panel_ui: &mut PanelUi, core: &mut ViewerCore) {
    panel_ui.fixed_time = None;
    panel_ui.stale = true;
    let _ = core.player.set_deterministic(false);
}

/// One frame of the app, on plain values so tests can drive it without a window.
pub fn frame(
    state: &mut ViewerState,
    panel_ui: &mut PanelUi,
    core: &mut ViewerCore,
    size: (u32, u32),
    scale: f32,
    safe: &SafeArea,
) {
    let _span = tracing::info_span!("viewer_host_frame").entered();
    state.stats.wakes += 1;
    let host = core.host.clone();
    if core.player.lazy().manual() {
        // Scripted capture: the panel and the picture move only in `scripted_step`, so how many app frames run
        // between two steps changes nothing.
        install_finished_package(state, core);
        drain_events(state, core);
        state.stats.wakes_per_second = wakes_per_second(false, state.task_running());
        return;
    }
    sync_input_and_panel(state, panel_ui, core, host.as_ref(), size, scale, safe);
    core.shift.set(piece_shift(
        Vec2::new(size.0 as f32 / scale, size.1 as f32 / scale),
        panel_ui.rect,
    ));
    let presents_before = host.as_ref().map_or(0, |h| h.borrow().presents());
    let now_ms = core.started.elapsed().as_secs_f64() * 1000.0;
    let moving = core.player.animating();
    // A frame is due and the player has no product to draw it with: the panel must still reach the screen,
    // or a refused package (or an empty store) would leave it black with the reason only in the log.
    let panel_only = core.player.lazy().pending() && !core.player.has_product();
    let _ = core.player.frame(now_ms);
    if panel_only
        && let Some(host) = &host
        && let Err(err) = host.borrow_mut().present_panel_only()
    {
        state.report_error(err.code, &err.message);
    }
    drain_events(state, core);
    let mut pending = core.player.lazy().pending();
    if let Some(host) = &host {
        let mut host = host.borrow_mut();
        if let Some(capture) = host.take_capture() {
            core.last_capture = Some(capture);
        }
        let presents = host.presents();
        if presents != presents_before {
            let now = Instant::now();
            state.stats.note_present(now, moving);
            if let Some(started) = core.switch_started.take() {
                // Until the GPU finished the frame: what a person waits for.
                host.wait_idle();
                state.stats.note_switch(started.elapsed());
            }
        }
        if host.take_retry() {
            drop(host);
            pending = core.player.invalidate() == Schedule::Request || core.player.lazy().pending();
        }
    }
    if panel_ui.stale {
        pending = core.player.invalidate() == Schedule::Request || core.player.lazy().pending();
    }
    let wakes = wakes_per_second(pending, state.task_running());
    state.stats.wakes_per_second = wakes;
    let presents = host.as_ref().map_or(0, |h| h.borrow().presents());
    state.stats.log_when_idle(presents, wakes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_piece_moves_to_the_middle_of_what_the_panel_leaves() {
        let window = Vec2::new(400.0, 800.0);
        let below = Rect::from_min_size(Pos2::new(0.0, 400.0), Vec2::new(400.0, 400.0));
        assert_eq!(piece_shift(window, Some(below)), [0.0, 0.25]);
        let wide = Vec2::new(800.0, 400.0);
        let beside = Rect::from_min_size(Pos2::new(600.0, 0.0), Vec2::new(200.0, 400.0));
        assert_eq!(piece_shift(wide, Some(beside)), [-0.125, 0.0]);
        assert_eq!(piece_shift(window, None), [0.0, 0.0]);
        assert_eq!(piece_shift(Vec2::ZERO, Some(below)), [0.0, 0.0]);
    }
}
