// SPDX-License-Identifier: MPL-2.0
//! `regia-editor` binary entry point (M10: the full editor shell).
//!
//! Owns the window (winit + wgpu + egui), the **edit world** (an `App` with the agent protocol
//! server and the editor's resources) and — with `--game <crate>` — a [`HotReloadHost`] whose
//! status shows in the toolbar (the M3 slice; the play-in-editor worlds arrive with `specs/19`
//! § 4 in M11). Agent `seeing.screenshot` requests are served from this window's present.

use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use regia_app::prelude::*;
use regia_editor::command::UndoStack;
use regia_editor::pie::{PieRequests, PieSession, PlayMode};
use regia_editor::play_window::{PlayWindow, PlayWindowEvent, PlayWindowGeometry, PlayWindowPlatform};
use regia_editor::shell::EditorShell;
use regia_editor::viewport::ViewportSettings;
use regia_editor::{HotReloadConfig, HotReloadHost, agent_ext};

/// Editor state: the shell, the edit world's app, the hot-reload host and the PIE session.
struct AppState {
    shell: EditorShell,
    app: App,
    frame_count: u64,
    exit_after: Option<u64>,
    host: Option<HotReloadHost>,
    /// The play-in-editor session (`None` while stopped). The session owns the play world app
    /// (`App` is not `Send`, so it lives here, not in a resource — see `regia_editor::pie`).
    pie: Option<PieSession>,
    /// The window entity the agent protocol addresses screenshots to.
    window_marker: Entity,
    /// Set when the shell decided the app should exit (checked in `about_to_wait`, which
    /// owns the event loop).
    exit_requested: bool,
    restart_normally: bool,
}

/// Wgpu rendering resources.
struct WgpuResources {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    egui_renderer: egui_wgpu::Renderer,
    /// The stand-in target frames draw into while the OS refuses the surface as occluded (ADR-7335),
    /// kept while its size and format match.
    stand_in: Option<wgpu::Texture>,
}

impl WgpuResources {
    /// The stand-in target at the surface's configured size and format (created or reused).
    fn stand_in_target(&mut self) -> wgpu::Texture {
        let (width, height) = (self.config.width.max(1), self.config.height.max(1));
        if let Some(texture) = &self.stand_in
            && texture.width() == width
            && texture.height() == height
            && texture.format() == self.config.format
        {
            return texture.clone();
        }
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("editor occluded stand-in"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        self.stand_in = Some(texture.clone());
        texture
    }
}

/// Whether a refused acquire still draws the frame, into the stand-in target: the OS reports the
/// window occluded (a locked console, a sleeping display) or gave no texture in time. The UI, the
/// viewport panes and agent screenshots keep working; nothing is presented (ADR-7335). Before, the
/// whole frame was skipped and `seeing.screenshot` waited until it timed out (F0006).
fn draws_into_stand_in(acquired: &wgpu::CurrentSurfaceTexture) -> bool {
    matches!(
        acquired,
        wgpu::CurrentSurfaceTexture::Occluded | wgpu::CurrentSurfaceTexture::Timeout
    )
}

/// Whether `about_to_wait` asks for a redraw. A realtime viewport or a playing session redraws at full
/// rate, but not while the surface is occluded: macOS answers redraws of an occluded window at once,
/// so that would spin. Egui repaint requests and the 1 Hz keep-alive draw either way.
fn editor_redraw_due(
    occluded: bool,
    full_rate: bool,
    egui_repaint: bool,
    since_last: std::time::Duration,
) -> bool {
    (full_rate && !occluded) || egui_repaint || since_last >= std::time::Duration::from_secs(1)
}

/// Window state: egui + wgpu + app data.
///
/// `wgpu.surface` holds a clone of `window`, so the window cannot die first; [`Drop`] still releases
/// them in that order, before the clipboard owner, whatever the field order.
struct EditorState {
    app: AppState,
    window: Option<Arc<Window>>,
    /// The main window's state has not been recorded since it was created (ADR-7753).
    observe_pending: bool,
    wgpu: Option<WgpuResources>,
    egui_ctx: egui::Context,
    egui_winit: Option<egui_winit::State>,
    /// When the last on-screen frame was drawn (the idle keep-alive paces against this).
    last_redraw: std::time::Instant,
    /// The last frame's surface was refused as occluded (it drew into the stand-in target).
    occluded: bool,
    /// The editor's GPU device (instance, adapter, device, queue): the play window's surface is
    /// made on it.
    render_device: Option<regia_render::RenderDevice>,
    /// The separate play window's state machine (`regia_editor::play_window`, ADR-7272).
    play_window: PlayWindow,
    /// The play window's OS window, surface and blit while it is open.
    play_gpu: Option<PlayWindowGpu>,
    /// The modifiers held over the play window (its key events carry them).
    play_modifiers: winit::keyboard::ModifiersState,
    /// When the editor frame last ran: the play window drives it when the editor window's redraws
    /// stop, as a compositor does for a hidden window (`regia_editor::play_window::FrameDriver`).
    frame_driver: regia_editor::play_window::FrameDriver,
    /// Floated panels' OS windows (ADR-7821).
    os_windows: Vec<OsWindowGpu>,
    /// The child viewports the last root pass announced, with their builders.
    os_viewports: Vec<(egui::ViewportId, egui::ViewportBuilder)>,
    /// Whether the last frame's root pass ran (its children only run after it).
    root_pass_ran: bool,
    /// A drag payload released outside its window, waiting for another window (ADR-7821).
    drop_carry: regia_editor::os_windows::DropCarry,
    /// The pointer over the main window, window pixels.
    main_cursor: Option<[f64; 2]>,
}

/// The separate play window's OS window and its own surface on the editor's shared device
/// (`specs/19` § 4, ADR-7272). A resize reconfigures the surface and a lost surface is made again
/// on the same window; the device and every other GPU resource stay. `surface` is declared first so
/// it drops before the window it was made from (it also holds its own clone of the window).
struct PlayWindowGpu {
    surface: wgpu::Surface<'static>,
    window: Arc<Window>,
    config: wgpu::SurfaceConfiguration,
    /// The format the blit writes: the surface format without its sRGB suffix, because the play
    /// world's picture already holds sRGB-encoded bytes.
    view_format: wgpu::TextureFormat,
    /// Scales the play world's picture (at the chosen play resolution) into its letterbox rect
    /// (ADR-7283).
    blitter: regia_editor::play_blit::LetterboxBlit,
    device_overlay: regia_editor::play_device::DeviceOverlay,
}

impl PlayWindowGpu {
    /// A surface for `window` on `device`, configured at the window's size.
    fn new(device: &regia_render::RenderDevice, window: Arc<Window>) -> Result<Self, String> {
        let surface = device
            .instance
            .create_surface(Arc::clone(&window))
            .map_err(|e| format!("play window surface: {e}"))?;
        let capabilities = surface.get_capabilities(&device.adapter);
        // A non-sRGB format takes the encoded bytes as they are; an sRGB-only surface gets a
        // non-sRGB view of the same texture.
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .or_else(|| capabilities.formats.first().copied())
            .ok_or_else(|| "the play window's surface exposes no texture formats".to_owned())?;
        let view_format = format.remove_srgb_suffix();
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: if view_format == format {
                vec![]
            } else {
                vec![view_format]
            },
        };
        surface.configure(&device.device, &config);
        let blitter = regia_editor::play_blit::LetterboxBlit::new(device, view_format);
        Ok(Self {
            surface,
            window,
            config,
            view_format,
            blitter,
            device_overlay: regia_editor::play_device::DeviceOverlay::new(device, view_format),
        })
    }
}

/// [`PlayWindowPlatform`] over the editor's event loop, device and the play window's GPU state.
struct WinitPlayWindow<'a> {
    event_loop: &'a ActiveEventLoop,
    device: Option<&'a regia_render::RenderDevice>,
    gpu: &'a mut Option<PlayWindowGpu>,
}

impl PlayWindowPlatform for WinitPlayWindow<'_> {
    fn open(&mut self, geometry: PlayWindowGeometry) -> Result<u64, String> {
        let device = self
            .device
            .ok_or_else(|| "the editor has no GPU device for a play window".to_owned())?;
        #[cfg(target_os = "linux")]
        use winit::platform::wayland::WindowAttributesExtWayland;
        let mut attributes = Window::default_attributes()
            .with_title(regia_editor::play_window::PLAY_WINDOW_TITLE)
            .with_window_icon(regia_editor::app_icon::window_icon())
            .with_inner_size(winit::dpi::PhysicalSize::new(geometry.width, geometry.height));
        if let Some([x, y]) = geometry.position {
            attributes = attributes.with_position(winit::dpi::PhysicalPosition::new(x, y));
        }
        // The same app-id family as the editor window, so the owner's window rule places both.
        #[cfg(target_os = "linux")]
        {
            attributes = attributes.with_name("regia.editor-play", "");
        }
        let window = Arc::new(
            self.event_loop
                .create_window(attributes)
                .map_err(|e| format!("play window: {e}"))?,
        );
        let id = u64::from(window.id());
        *self.gpu = Some(PlayWindowGpu::new(device, window)?);
        Ok(id)
    }

    fn configure_surface(&mut self, width: u32, height: u32) {
        if let (Some(gpu), Some(device)) = (self.gpu.as_mut(), self.device) {
            gpu.config.width = width.max(1);
            gpu.config.height = height.max(1);
            gpu.surface.configure(&device.device, &gpu.config);
        }
    }

    fn recreate_surface(&mut self) -> Result<(), String> {
        let (Some(gpu), Some(device)) = (self.gpu.as_mut(), self.device) else {
            return Err("no play window".to_owned());
        };
        let size = gpu.window.inner_size();
        gpu.config.width = size.width.max(1);
        gpu.config.height = size.height.max(1);
        // The old surface goes first; the new one is made on the same window and device.
        gpu.surface = device
            .instance
            .create_surface(Arc::clone(&gpu.window))
            .map_err(|e| format!("play window surface: {e}"))?;
        gpu.surface.configure(&device.device, &gpu.config);
        Ok(())
    }

    fn set_capture(&mut self, captured: bool) {
        let Some(gpu) = self.gpu.as_ref() else {
            return;
        };
        if captured {
            let grabbed = gpu
                .window
                .set_cursor_grab(winit::window::CursorGrabMode::Confined)
                .or_else(|_| gpu.window.set_cursor_grab(winit::window::CursorGrabMode::Locked));
            if let Err(e) = grabbed {
                tracing::warn!(target: "regia_editor", "cannot capture the cursor in the play window: {e}");
            }
            gpu.window.set_cursor_visible(false);
        } else {
            let _ = gpu.window.set_cursor_grab(winit::window::CursorGrabMode::None);
            gpu.window.set_cursor_visible(true);
        }
    }

    fn close(&mut self) {
        // The surface drops before the window (field order, and it holds its own clone).
        *self.gpu = None;
    }
}

/// A winit event of the play window in the engine's terms (`None`: nothing the play window uses).
fn play_window_event(
    event: &WindowEvent,
    modifiers: winit::keyboard::ModifiersState,
) -> Option<PlayWindowEvent> {
    Some(match event {
        WindowEvent::KeyboardInput { event, .. } => PlayWindowEvent::Key {
            physical: match event.physical_key {
                winit::keyboard::PhysicalKey::Code(code) => format!("{code:?}"),
                winit::keyboard::PhysicalKey::Unidentified(native) => format!("Unidentified({native:?})"),
            },
            logical: match &event.logical_key {
                Key::Character(text) => text.to_string(),
                Key::Named(named) => format!("{named:?}"),
                Key::Unidentified(native) => format!("Unidentified({native:?})"),
                Key::Dead(c) => format!("Dead({c:?})"),
            },
            pressed: event.state == ElementState::Pressed,
            repeat: event.repeat,
            shift: modifiers.shift_key(),
            ctrl: modifiers.control_key(),
            alt: modifiers.alt_key(),
        },
        WindowEvent::MouseInput { state, button, .. } => PlayWindowEvent::MouseButton {
            button: match button {
                winit::event::MouseButton::Left => regia_platform::MouseButton::Left,
                winit::event::MouseButton::Right => regia_platform::MouseButton::Right,
                winit::event::MouseButton::Middle => regia_platform::MouseButton::Middle,
                winit::event::MouseButton::Back => regia_platform::MouseButton::Back,
                winit::event::MouseButton::Forward => regia_platform::MouseButton::Forward,
                winit::event::MouseButton::Other(n) => regia_platform::MouseButton::Other(*n),
            },
            pressed: *state == ElementState::Pressed,
        },
        WindowEvent::CursorMoved { position, .. } => PlayWindowEvent::CursorMoved {
            x: position.x,
            y: position.y,
        },
        WindowEvent::MouseWheel { delta, .. } => match delta {
            winit::event::MouseScrollDelta::LineDelta(x, y) => PlayWindowEvent::Wheel {
                delta: (*x, *y),
                unit: regia_platform::WheelUnit::Line,
            },
            winit::event::MouseScrollDelta::PixelDelta(p) => PlayWindowEvent::Wheel {
                delta: (p.x as f32, p.y as f32),
                unit: regia_platform::WheelUnit::Pixel,
            },
        },
        WindowEvent::Focused(focused) => PlayWindowEvent::Focused(*focused),
        WindowEvent::Resized(size) => PlayWindowEvent::Resized {
            width: size.width,
            height: size.height,
        },
        WindowEvent::Moved(position) => PlayWindowEvent::Moved {
            x: position.x,
            y: position.y,
        },
        WindowEvent::CloseRequested => PlayWindowEvent::CloseRequested,
        _ => return None,
    })
}

impl EditorState {
    /// Applies a play input capture event (`specs/19` § 4, `regia_editor::play_input`): grabs and
    /// hides the cursor on capture, frees and shows it on release.
    fn apply_play_capture(&mut self, event: regia_editor::play_input::CaptureEvent) {
        let session_active = self.app.pie.is_some();
        let world = self.app.app.world_mut();
        let mut capture = world
            .get_resource::<regia_editor::play_input::PlayInputCapture>()
            .copied()
            .unwrap_or_default();
        let changed = regia_editor::play_input::on_event(&mut capture, session_active, event);
        world.insert_resource(capture);
        let (Some(grab), Some(window)) = (changed, &self.window) else {
            return;
        };
        if grab {
            // Confined where the platform has it, locked otherwise (macOS has no confine).
            let grabbed = window
                .set_cursor_grab(winit::window::CursorGrabMode::Confined)
                .or_else(|_| window.set_cursor_grab(winit::window::CursorGrabMode::Locked));
            if let Err(e) = grabbed {
                tracing::warn!(target: "regia_editor", "cannot capture the cursor for play: {e}");
            }
            window.set_cursor_visible(false);
            self.app
                .shell
                .set_toast("mouse captured by play — Shift+F1 releases it".to_string());
        } else {
            let _ = window.set_cursor_grab(winit::window::CursorGrabMode::None);
            window.set_cursor_visible(true);
        }
    }

    fn new(app: AppState) -> Self {
        Self {
            app,
            window: None,
            observe_pending: false,
            wgpu: None,
            egui_ctx: egui::Context::default(),
            egui_winit: None,
            last_redraw: std::time::Instant::now(),
            render_device: None,
            play_window: PlayWindow::default(),
            play_gpu: None,
            play_modifiers: winit::keyboard::ModifiersState::empty(),
            frame_driver: regia_editor::play_window::FrameDriver::default(),
            occluded: false,
            os_windows: Vec::new(),
            os_viewports: Vec::new(),
            root_pass_ran: false,
            drop_carry: regia_editor::os_windows::DropCarry::default(),
            main_cursor: None,
        }
    }

    /// Routes one event of a window that is not the editor's to the play window (ADR-7272).
    fn on_play_window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: &WindowEvent,
    ) {
        if let WindowEvent::ModifiersChanged(modifiers) = event {
            self.play_modifiers = modifiers.state();
            return;
        }
        if matches!(event, WindowEvent::RedrawRequested) {
            // A compositor stops the redraws of a hidden editor window (the play window may cover
            // it); the play window then runs the editor frame itself, so play keeps stepping.
            if self.play_window.id() == Some(u64::from(window_id))
                && self
                    .frame_driver
                    .play_redraw_runs_frame(std::time::Instant::now(), true)
                && let Some(main) = self.window.as_ref().map(|w| w.id())
            {
                self.window_event(event_loop, main, WindowEvent::RedrawRequested);
            }
            return;
        }
        let Some(event) = play_window_event(event, self.play_modifiers) else {
            return;
        };
        self.deliver_play_window_event(event_loop, u64::from(window_id), event);
    }

    fn deliver_play_window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window: u64,
        event: PlayWindowEvent,
    ) {
        let mut platform = WinitPlayWindow {
            event_loop,
            device: self.render_device.as_ref(),
            gpu: &mut self.play_gpu,
        };
        let was_captured = self.play_window.captured();
        let routed = self.play_window.on_event(
            window,
            event,
            self.app.app.world_mut(),
            &mut self.app.pie,
            &mut platform,
        );
        match routed {
            regia_editor::play_window::Routed::Closed => {
                self.app
                    .shell
                    .set_toast("⏹ play window closed: play stopped".to_string());
            }
            regia_editor::play_window::Routed::Play if self.play_window.captured() && !was_captured => {
                self.app
                    .shell
                    .set_toast("mouse captured by the play window — Shift+F1 releases it".to_string());
            }
            _ => {}
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Opens, moves or closes the play window after the frame's play requests (ADR-7272).
    fn sync_play_window(&mut self, event_loop: &ActiveEventLoop) {
        let mut platform = WinitPlayWindow {
            event_loop,
            device: self.render_device.as_ref(),
            gpu: &mut self.play_gpu,
        };
        self.play_window
            .sync(self.app.app.world_mut(), self.app.pie.as_mut(), &mut platform);
    }

    /// Draws the play world's camera into the play window: the frame renders offscreen at the
    /// chosen play resolution on the shared device and a letterbox blit scales it onto the window's
    /// surface. What to do with the surface is `PlayWindow::begin_frame`'s decision (a lost or
    /// outdated surface is recreated, only the surface, and the frame skipped; tests drive it).
    fn render_play_window(&mut self, event_loop: &ActiveEventLoop) {
        let Some(gpu) = self.play_gpu.as_mut() else {
            return;
        };
        if self.render_device.is_none() || self.app.pie.is_none() {
            return;
        }
        let _span = tracing::info_span!(target: "regia_editor", "play_window_present").entered();
        let (output, acquire) = match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(tex) | wgpu::CurrentSurfaceTexture::Suboptimal(tex) => {
                (Some(tex), regia_editor::play_window::SurfaceAcquire::Ready)
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                (None, regia_editor::play_window::SurfaceAcquire::Lost)
            }
            // Timeout, Occluded, Validation: skip the frame.
            _ => (None, regia_editor::play_window::SurfaceAcquire::Unavailable),
        };
        let mut platform = WinitPlayWindow {
            event_loop,
            device: self.render_device.as_ref(),
            gpu: &mut self.play_gpu,
        };
        let decision = self.play_window.begin_frame(
            acquire,
            self.app.app.world_mut(),
            &mut self.app.pie,
            &mut platform,
        );
        let (regia_editor::play_window::PlayFrame::Render { size, picture }, Some(output)) =
            (decision, output)
        else {
            return;
        };
        let (Some(gpu), Some(device), Some(session)) = (
            self.play_gpu.as_mut(),
            self.render_device.as_ref(),
            self.app.pie.as_mut(),
        ) else {
            return;
        };
        let possess = session.options.kind == regia_editor::pie::PlayKind::Play;
        let Some(frame) =
            regia_editor::render::render_play_window_gpu(device, session.app.world_mut(), size, possess)
        else {
            return;
        };
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(gpu.view_format),
            ..Default::default()
        });
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("play window blit"),
            });
        gpu.blitter
            .draw(device, &mut encoder, &frame.target.view, &view, picture);
        self.play_window.draw_device_overlay(
            &mut gpu.device_overlay,
            device,
            &mut encoder,
            &view,
            self.app.app.world(),
            gpu.window.scale_factor() as f32,
        );
        device.queue.submit([encoder.finish()]);
        gpu.window.pre_present_notify();
        device.queue.present(output);
        self.play_window.presented(self.app.app.world_mut());
    }

    fn init_wgpu(&mut self, event_loop: &ActiveEventLoop) {
        // The Wayland app_id (`app_icon::APP_ID`) matches the owner's window rule
        // (`class = "^(regia\..*)$"`), so the editor always lands on the dedicated dev workspace,
        // never the active one, and equals the desktop entry's name, so docks show its icon
        // (ADR-20000). The window and taskbar icon come from the editor's icon set.
        let mut attributes = regia_editor::app_icon::with_app_identity(
            Window::default_attributes().with_title("Regia Engine Editor"),
        );
        // The project's saved window state (ADR-7753): monitor, size, position, full screen; a
        // monitor that is gone falls back to the primary display.
        if let Some((placement, monitors)) = saved_placement(event_loop, &self.app) {
            attributes = attributes
                .with_inner_size(winit::dpi::PhysicalSize::new(placement.size.0, placement.size.1))
                .with_maximized(placement.maximized);
            if let Some((x, y)) = placement.position {
                attributes = attributes.with_position(winit::dpi::PhysicalPosition::new(x, y));
            }
            if placement.fullscreen {
                let monitor = placement.monitor.and_then(|i| monitors.get(i).cloned());
                attributes = attributes.with_fullscreen(Some(winit::window::Fullscreen::Borderless(monitor)));
            }
        }
        // The screen-reader adapter (ADR-7746): Windows and macOS subclass the window before it is
        // first shown, so it starts hidden there and shows once the adapter is attached.
        let hidden = regia_editor::a11y_bridge::create_window_hidden();
        if hidden {
            attributes = attributes.with_visible(false);
        }
        let window = Arc::new(event_loop.create_window(attributes).unwrap_or_else(|e| {
            eprintln!("regia-editor: failed to create window: {e}");
            std::process::exit(1);
        }));
        regia_editor::a11y_bridge::attach_window(self.app.app.world_mut(), &window);
        if hidden {
            window.set_visible(true);
        }

        let size = window.inner_size();

        // Blocking helper for wgpu's async API — the native backend usually completes at once.
        // A pending future parks the thread (woken by the future, or after 1 ms in case it
        // never wakes) instead of spinning a core at 100 % (audit 11-03 editor L5).
        fn block_on<F: std::future::Future>(f: F) -> F::Output {
            struct ThreadWaker(std::thread::Thread);
            impl std::task::Wake for ThreadWaker {
                fn wake(self: Arc<Self>) {
                    self.0.unpark();
                }
            }
            let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
            let mut cx = Context::from_waker(&waker);
            let mut future = pin!(f);
            loop {
                match future.as_mut().poll(&mut cx) {
                    Poll::Ready(val) => return val,
                    Poll::Pending => std::thread::park_timeout(std::time::Duration::from_millis(1)),
                }
            }
        }

        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = regia_render::instance_backends();
        // Under the engine's loader-list lock, like every engine instance (ADR-2113): the editor
        // opens headless devices of its own (previews, bakes) on other threads.
        let instance = regia_render::create_window_instance(descriptor);

        // A 'static surface: it owns a clone of the window, so both can live in the
        // same struct without a self-referential borrow, and the window cannot die first.
        let surface = match instance.create_surface(Arc::clone(&window)) {
            Ok(surface) => surface,
            Err(err) => {
                eprintln!("regia-editor: failed to create a GPU surface: {err}");
                std::process::exit(1);
            }
        };

        // `REGIA_GPU_*` picks the card here as it does for every headless tool; without a
        // variable the default hardware pick (Vulkan first) applies, then wgpu's own choice.
        let chosen = match regia_render::adapter_for_surface(&instance, &surface) {
            Ok((chosen, selection)) => {
                tracing::info!(target: "regia_editor", "adapter selection: {selection:?}");
                chosen
            }
            Err(err) => {
                eprintln!("regia-editor: {err}");
                std::process::exit(1);
            }
        };
        let adapter = match chosen.map_or_else(
            || {
                block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                    compatible_surface: Some(&surface),
                    ..Default::default()
                }))
            },
            Ok,
        ) {
            Ok(adapter) => adapter,
            Err(err) => {
                eprintln!("regia-editor: no suitable GPU adapter: {err}");
                std::process::exit(1);
            }
        };

        let info = adapter.get_info();
        regia_platform::crash::set_gpu_info(format!(
            "{} ({:?}; driver {} {})",
            info.name, info.backend, info.driver, info.driver_info
        ));

        // The same lock for the device (ADR-2113): `vkCreateDevice` links it into the loader's
        // lists, which a concurrent engine creation walks.
        let (device, queue) = match regia_render::request_device_locked(
            &adapter,
            &wgpu::DeviceDescriptor {
                // The engine's own device path asks for the adapter's limits (see
                // `RenderDevice::new`); the viewport's forward pass needs the same headroom.
                required_limits: adapter.limits(),
                // The same optional features `RenderDevice::new` asks for: without them every
                // `fallback_*` problem fires on a capable adapter (v0.2.4 retest on RADV).
                required_features: regia_render::baseline_optional_features(adapter.features()),
                ..Default::default()
            },
        ) {
            Ok((device, queue)) => (device, queue),
            Err(err) => {
                eprintln!("regia-editor: GPU device creation failed: {err}");
                std::process::exit(1);
            }
        };

        let capabilities = surface.get_capabilities(&adapter);
        let Some(format) = capabilities
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .or_else(|| capabilities.formats.first().copied())
        else {
            eprintln!("regia-editor: the surface exposes no texture formats");
            std::process::exit(1);
        };

        // Screenshots copy the presented frame out; request COPY_SRC when the platform allows.
        let mut usage = wgpu::TextureUsages::RENDER_ATTACHMENT;
        if capabilities.usages.contains(wgpu::TextureUsages::COPY_SRC) {
            usage |= wgpu::TextureUsages::COPY_SRC;
        }

        let config = wgpu::SurfaceConfiguration {
            usage,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: size.width,
            height: size.height,
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let egui_renderer = egui_wgpu::Renderer::new(&device, format, Default::default());

        // The viewport renders through the engine's frame renderer on the same device.
        let capabilities = regia_render::RenderCapabilities::from_adapter(&adapter, &device);
        let render_device = regia_render::RenderDevice::from_parts(
            instance,
            adapter,
            device.clone(),
            queue.clone(),
            capabilities,
        );
        self.render_device = Some(render_device.clone());
        self.app.shell = EditorShell::new(Some(render_device), self.app.shell.project_root());
        self.app
            .shell
            .set_pending_reports(regia_editor::crash_reports::list());
        // A window system is here: `editor.play { window: true }` may open a play window (ADR-7272).
        self.app
            .app
            .world_mut()
            .insert_resource(regia_editor::play_window::PlayWindowSupport);
        // Viewport panes are shown from GPU textures registered with this egui renderer
        // (ADR-1555); REGIA_EDITOR_PANE_PATH=cpu keeps the readback path.
        self.app
            .app
            .world_mut()
            .insert_resource(regia_editor::pane_texture::PaneTextures::new(
                regia_editor::pane_texture::PanePath::from_env(),
            ));

        self.wgpu = Some(WgpuResources {
            surface,
            device,
            queue,
            config,
            egui_renderer,
            stand_in: None,
        });

        self.egui_winit = Some(egui_winit::State::new(
            self.egui_ctx.clone(),
            self.egui_ctx.viewport_id(),
            &window,
            None,
            None,
            None,
        ));

        // Floated panels may be OS windows (ADR-7821): this runner opens egui's child viewports.
        self.egui_ctx.set_embed_viewports(false);
        let monitors = os_monitor_rects(event_loop);
        self.app
            .app
            .world_mut()
            .insert_resource(regia_editor::os_windows::OsWindowSupport { monitors });

        self.window = Some(window);
        self.observe_pending = true;
    }

    fn render(&mut self) {
        self.root_pass_ran = false;
        // Collect data from the window before borrowing self for UI.
        let scale_factor;
        let mut raw_input;
        {
            let Some(window) = &self.window else {
                return;
            };
            scale_factor = window.scale_factor() as f32;
            let Some(egui_winit) = &mut self.egui_winit else {
                return;
            };
            raw_input = egui_winit.take_egui_input(window);
            // The main window's desktop rectangle: an OS window opens relative to it (ADR-7821).
            let info = raw_input.viewports.entry(egui::ViewportId::ROOT).or_default();
            egui_winit::update_viewport_info(info, &self.egui_ctx, window, false);
        }

        // Take the wgpu resources out for the whole frame: the surface texture acquired below
        // must outlive the UI phase, which borrows self.app (a disjoint field, but the borrow
        // checker cannot see that through &mut self.wgpu).
        let mut wgpu = match self.wgpu.take() {
            Some(wgpu) => wgpu,
            None => return,
        };
        // Every path out of this scope must put the resources back; the inner function returns
        // `Result<(), ()>` only so `?` cannot skip the restore below.
        let _ = (|| -> Result<(), ()> {
            // The current surface texture is the frame's source of truth for the window size:
            // wgpu auto-resizes the surface to the window, so the texture — not the stale
            // creation-time config — tells egui how big the screen is (round-2 fix for panels
            // covering only part of tall/resized windows).
            let acquired = wgpu.surface.get_current_texture();
            self.occluded = draws_into_stand_in(&acquired);
            let (target, output) = match acquired {
                wgpu::CurrentSurfaceTexture::Success(tex) | wgpu::CurrentSurfaceTexture::Suboptimal(tex) => {
                    (tex.texture.clone(), Some(tex))
                }
                refused if draws_into_stand_in(&refused) => (wgpu.stand_in_target(), None),
                wgpu::CurrentSurfaceTexture::Outdated => {
                    if let Some(window) = &self.window {
                        let size = window.inner_size();
                        wgpu.config.width = size.width.max(1);
                        wgpu.config.height = size.height.max(1);
                    }
                    wgpu.surface.configure(&wgpu.device, &wgpu.config);
                    return Ok(());
                }
                _ => return Ok(()), // Lost, Validation: skip the frame.
            };
            let width = target.width();
            let height = target.height();
            // Bookkeeping only: wgpu auto-resizes the surface with the window, so the config
            // just records what the surface already is (used when re-establishing after loss).
            wgpu.config.width = width;
            wgpu.config.height = height;

            // egui's screen follows the real surface, every frame, in points of the window's scale
            // factor times the UI-scale preference (egui's zoom; ADR-8589).
            regia_editor::editor_prefs::frame_screen(
                &self.egui_ctx,
                &mut raw_input,
                scale_factor,
                [width, height],
            );

            // Phase 1: the editor UI over the edit world (and, while playing, the play world).
            let egui_ctx = self.egui_ctx.clone();
            let mut egui_output = {
                let AppState {
                    shell,
                    app,
                    host,
                    pie,
                    ..
                } = &mut self.app;
                let _ = host.as_mut().map(|host| host.frame());
                regia_editor::asset_watch::pump_asset_watch(
                    app.world_mut(),
                    pie.as_mut().map(|session| session.app.world_mut()),
                );
                // Apply a finished DCC bake before the UI and before any agent poll this frame.
                regia_editor::dcc_feed::pump_bakes(app.world_mut());
                // Last frame's edits and undos feed the open level's dirty set (ADR-7203).
                regia_editor::level_dirty::feed_on_history_change(app.world_mut());
                let world = app.world_mut();
                let mut play_world = pie.as_mut().map(|session| session.app.world_mut());
                // Screen-reader requests become egui input; the frame's tree goes to the OS adapter (ADR-7746).
                regia_editor::a11y_bridge::before_frame(&egui_ctx, world, &mut raw_input);
                let mut output = egui_ctx.run_ui(raw_input, |ui| {
                    shell.ui(ui, world, play_world.as_deref_mut());
                });
                regia_editor::a11y_bridge::after_frame(&egui_ctx, world, &mut output.platform_output);
                // Panels can queue per-frame side effects (viewport drops).
                shell.frame_effects(&egui_ctx, world);
                // The unsaved-changes modal may have collected a decision (specs/19 § 7).
                self.app.apply_close_decision();
                output
            };

            // The child viewports this pass announced: the floated panels' OS windows (ADR-7821).
            self.os_viewports.clear();
            for (id, viewport) in &egui_output.viewport_output {
                if *id != egui::ViewportId::ROOT {
                    self.os_viewports.push((*id, viewport.builder.clone()));
                }
            }
            self.root_pass_ran = true;

            // Handle platform output.
            if let (Some(window), Some(egui_winit)) = (&self.window, &mut self.egui_winit) {
                egui_winit.handle_platform_output(window, egui_output.platform_output.clone());
            }
            // Each OS window's pass, before any texture is applied or freed (ADR-7821).
            let os_frames = self.run_os_window_passes();

            let mut paint_jobs = self
                .egui_ctx
                .tessellate(egui_output.shapes, egui_output.pixels_per_point);
            // View ▸ Color Vision filters the chrome's vertex colors (ADR-7838).
            regia_editor::chrome_color_vision::filter_paint_jobs(self.app.app.world(), &mut paint_jobs);

            // Phase 2: wgpu rendering. Texture deltas are applied (or cleared) before any
            // early return — dropping unapplied deltas panics in egui.
            let screen_descriptor = egui_wgpu::ScreenDescriptor {
                size_in_pixels: [width, height],
                // The pass's own scale: native x `appearance.ui_scale` (ADR-8589).
                pixels_per_point: egui_output.pixels_per_point,
            };

            for (id, deltas) in &egui_output.textures_delta.set {
                for delta in deltas {
                    wgpu.egui_renderer
                        .update_texture(&wgpu.device, &wgpu.queue, *id, delta);
                }
            }
            // The OS windows' passes may have grown shared textures (the font atlas).
            for frame in &os_frames {
                for (id, deltas) in &frame.output.textures_delta.set {
                    for delta in deltas {
                        wgpu.egui_renderer
                            .update_texture(&wgpu.device, &wgpu.queue, *id, delta);
                    }
                }
            }

            // Pane textures: register what the panes submitted this frame, free the rest.
            let pane_textures = self
                .app
                .app
                .world_mut()
                .get_resource::<regia_editor::pane_texture::PaneTextures>()
                .cloned();
            if let Some(textures) = pane_textures {
                textures.apply(&mut regia_editor::pane_texture::EguiNative {
                    renderer: &mut wgpu.egui_renderer,
                    device: &wgpu.device,
                });
            }

            let view = target.create_view(&wgpu::TextureViewDescriptor::default());

            let mut encoder = wgpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("egui encoder"),
                });

            wgpu.egui_renderer.update_buffers(
                &wgpu.device,
                &wgpu.queue,
                &mut encoder,
                &paint_jobs,
                &screen_descriptor,
            );

            // Agent screenshot requests are served from this present: copy the frame out before
            // presenting, read it back, hand it to the callbacks (specs/23 § 3.4).
            let callbacks = {
                let AppState {
                    app, window_marker, ..
                } = &mut self.app;
                let world = app.world_mut();
                if world.get_resource::<regia_render::ScreenshotRequests>().is_none() {
                    world.init_resource::<regia_render::ScreenshotRequests>();
                }
                world
                    .resource_mut::<regia_render::ScreenshotRequests>()
                    .take_for(*window_marker)
            };
            let mut readbacks: Vec<(wgpu::Buffer, u32, u32, u32)> = Vec::new();
            for _ in &callbacks {
                let (width, height) = (target.width(), target.height());
                let padded_row = (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
                    * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
                let buffer = wgpu.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("editor screenshot"),
                    size: u64::from(padded_row) * u64::from(height),
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                // The copy itself is encoded after the render pass below, so it captures the
                // finished frame (commands in one encoder run in order).
                readbacks.push((buffer, width, height, padded_row));
            }

            // The render pass borrows encoder; forget the lifetime for egui's 'static
            // requirement, then drop the pass before encoder.finish().
            {
                let mut rp = encoder
                    .begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("egui render pass"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color {
                                    r: 0.02,
                                    g: 0.02,
                                    b: 0.025,
                                    a: 1.0,
                                }),
                                store: wgpu::StoreOp::Store,
                            },
                            depth_slice: None,
                        })],
                        depth_stencil_attachment: None,
                        occlusion_query_set: None,
                        timestamp_writes: None,
                        multiview_mask: None,
                    })
                    .forget_lifetime();

                wgpu.egui_renderer
                    .render(&mut rp, &paint_jobs, &screen_descriptor);
            }

            // The frame is drawn: encode the screenshot copies now (after the pass, before
            // submit) so they capture the finished pixels.
            for (buffer, width, height, padded_row) in &readbacks {
                encoder.copy_texture_to_buffer(
                    wgpu::TexelCopyTextureInfo {
                        texture: &target,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::TexelCopyBufferInfo {
                        buffer,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(*padded_row),
                            rows_per_image: Some(*height),
                        },
                    },
                    wgpu::Extent3d {
                        width: *width,
                        height: *height,
                        depth_or_array_layers: 1,
                    },
                );
            }

            wgpu.queue.submit([encoder.finish()]);
            if let Some(output) = output {
                wgpu.queue.present(output);
            }

            for id in egui_output.textures_delta.free.iter() {
                wgpu.egui_renderer.free_texture(id);
            }
            // All deltas are applied; epaint's debug-drop check wants them drained too.
            egui_output.textures_delta.clear();
            // The OS windows draw after the main window, one after another (ADR-7821).
            self.paint_os_windows(&mut wgpu, os_frames);

            // Finish the screenshot readbacks and answer the callbacks.
            for ((buffer, width, height, padded_row), callback) in readbacks.into_iter().zip(callbacks) {
                let result = read_back(
                    &wgpu.device,
                    buffer,
                    width,
                    height,
                    padded_row,
                    wgpu.config.format,
                );
                callback(result);
            }
            Ok(())
        })();
        self.wgpu = Some(wgpu);
    }
}
// End of impl EditorState

/// The drop carry's key of the main window (an OS window's key is its viewport id's hash).
const MAIN_WINDOW_KEY: u64 = 0;

/// The release a carried drop lands with (ADR-7821).
fn carried_release(device_id: DeviceId) -> WindowEvent {
    WindowEvent::MouseInput {
        device_id,
        state: ElementState::Released,
        button: winit::event::MouseButton::Left,
    }
}

/// The monitors' rectangles `(x, y, width, height)`, physical pixels.
fn os_monitor_rects(event_loop: &ActiveEventLoop) -> Vec<[i32; 4]> {
    event_loop
        .available_monitors()
        .map(|monitor| {
            let at = monitor.position();
            let size = monitor.size();
            [
                at.x,
                at.y,
                i32::try_from(size.width).unwrap_or(i32::MAX),
                i32::try_from(size.height).unwrap_or(i32::MAX),
            ]
        })
        .collect()
}

/// A floated panel group's OS window (ADR-7821): the winit window, its egui input state and its
/// surface on the editor's device. `surface` is declared first so it drops before the window.
struct OsWindowGpu {
    surface: wgpu::Surface<'static>,
    window: Arc<Window>,
    config: wgpu::SurfaceConfiguration,
    egui_winit: egui_winit::State,
    viewport: egui::ViewportId,
    title: Option<String>,
    /// The pointer over this window, window pixels.
    cursor: Option<[f64; 2]>,
}

impl OsWindowGpu {
    /// Opens the window `builder` describes, with a surface in the egui renderer's `format`.
    fn open(
        ctx: &egui::Context,
        event_loop: &ActiveEventLoop,
        device: &regia_render::RenderDevice,
        viewport: egui::ViewportId,
        builder: &egui::ViewportBuilder,
        format: wgpu::TextureFormat,
    ) -> Result<Self, String> {
        let window = Arc::new(
            egui_winit::create_window(ctx, event_loop, builder).map_err(|e| format!("panel window: {e}"))?,
        );
        let surface = device
            .instance
            .create_surface(Arc::clone(&window))
            .map_err(|e| format!("panel window surface: {e}"))?;
        if !surface
            .get_capabilities(&device.adapter)
            .formats
            .contains(&format)
        {
            return Err(format!("the panel window's surface cannot show {format:?}"));
        }
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
        };
        surface.configure(&device.device, &config);
        let egui_winit = egui_winit::State::new(ctx.clone(), viewport, &window, None, None, None);
        Ok(Self {
            surface,
            window,
            config,
            egui_winit,
            viewport,
            title: builder.title.clone(),
            cursor: None,
        })
    }

    /// The drop carry's key of this window.
    fn key(&self) -> u64 {
        egui::Id::from(self.viewport).value()
    }
}

/// One OS window's finished pass, waiting for its paint.
struct OsFrame {
    at: usize,
    output: egui::FullOutput,
    texture: wgpu::SurfaceTexture,
    size: [u32; 2],
}

/// The floated panels' OS windows (ADR-7821): opened and closed after the viewports egui listed,
/// their passes run after the root pass, painted after the main window.
impl EditorState {
    /// Opens a window for every child viewport the last root pass listed and closes the rest. A
    /// restored window whose monitor is gone, or one that cannot open, docks back instead.
    fn sync_os_windows(&mut self, event_loop: &ActiveEventLoop) {
        let wanted = std::mem::take(&mut self.os_viewports);
        self.os_windows
            .retain(|window| wanted.iter().any(|(id, _)| *id == window.viewport));
        let (Some(device), Some(format)) = (
            self.render_device.clone(),
            self.wgpu.as_ref().map(|wgpu| wgpu.config.format),
        ) else {
            self.os_viewports = wanted;
            return;
        };
        let scale = self.window.as_ref().map_or(1.0, |w| w.scale_factor());
        for (id, builder) in &wanted {
            if let Some(open) = self.os_windows.iter_mut().find(|w| w.viewport == *id) {
                if builder.title != open.title {
                    if let Some(title) = &builder.title {
                        open.window.set_title(title);
                    }
                    open.title = builder.title.clone();
                }
                continue;
            }
            let serial = self.app.shell.os_window_serial(*id);
            if let Some(serial) = serial
                && self.app.shell.os_window_restored(serial)
            {
                if let (Some(pos), Some(size)) = (builder.position, builder.inner_size) {
                    let px = |v: f32| (f64::from(v) * scale) as i32;
                    let monitors = os_monitor_rects(event_loop);
                    if !regia_editor::os_windows::visible_on_monitors(
                        [px(pos.x), px(pos.y)],
                        [px(size.x), px(size.y)],
                        &monitors,
                    ) {
                        // Its monitor is gone: the window docks back (ADR-7821).
                        regia_editor::os_windows::OsWindowRequests::push(
                            self.app.app.world_mut(),
                            regia_editor::os_windows::OsWindowRequest::DockWindow(serial),
                        );
                        continue;
                    }
                }
                self.app.shell.os_window_placed(serial);
            }
            match OsWindowGpu::open(&self.egui_ctx, event_loop, &device, *id, builder, format) {
                Ok(window) => self.os_windows.push(window),
                Err(err) => {
                    tracing::warn!(target: "regia_editor", "{err}");
                    if let Some(serial) = serial {
                        regia_editor::os_windows::OsWindowRequests::push(
                            self.app.app.world_mut(),
                            regia_editor::os_windows::OsWindowRequest::DockWindow(serial),
                        );
                    }
                    self.app.shell.set_toast(format!("{err}: the panels docked back"));
                }
            }
        }
        self.os_viewports = wanted;
    }

    /// Runs each OS window's egui pass (after the root pass, before any paint).
    fn run_os_window_passes(&mut self) -> Vec<OsFrame> {
        let mut frames = Vec::new();
        if !self.root_pass_ran || self.os_windows.is_empty() {
            return frames;
        }
        let EditorState {
            os_windows,
            app,
            egui_ctx,
            render_device,
            os_viewports,
            ..
        } = self;
        for (at, window) in os_windows.iter_mut().enumerate() {
            // A window this frame's root pass no longer listed closes on the next frame: no pass.
            if !os_viewports.iter().any(|(id, _)| *id == window.viewport) {
                continue;
            }
            let _span = tracing::info_span!(target: "regia_editor", "os_window_pass").entered();
            let texture = match window.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(tex) | wgpu::CurrentSurfaceTexture::Suboptimal(tex) => {
                    tex
                }
                wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                    // A resized or lost surface is made again on the same window; the frame is skipped.
                    if let Some(device) = render_device.as_ref() {
                        let size = window.window.inner_size();
                        window.config.width = size.width.max(1);
                        window.config.height = size.height.max(1);
                        window.surface.configure(&device.device, &window.config);
                    }
                    continue;
                }
                _ => continue,
            };
            let size = [texture.texture.width(), texture.texture.height()];
            let ppp = egui_winit::pixels_per_point(egui_ctx, &window.window);
            let mut raw = window.egui_winit.take_egui_input(&window.window);
            let info = raw.viewports.entry(window.viewport).or_default();
            info.parent = Some(egui::ViewportId::ROOT);
            egui_winit::update_viewport_info(info, egui_ctx, &window.window, false);
            raw.screen_rect = Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(size[0] as f32 / ppp, size[1] as f32 / ppp),
            ));
            let AppState {
                shell,
                app: edit,
                pie,
                ..
            } = &mut *app;
            let world = edit.world_mut();
            let mut play = pie.as_mut().map(|session| session.app.world_mut());
            let viewport = window.viewport;
            let mut output = egui_ctx.run_ui(raw, |ui| {
                shell.os_window_ui(ui, viewport, world, play.as_deref_mut());
            });
            window
                .egui_winit
                .handle_platform_output(&window.window, std::mem::take(&mut output.platform_output));
            frames.push(OsFrame {
                at,
                output,
                texture,
                size,
            });
        }
        frames
    }

    /// Paints the OS windows' passes with the shared egui renderer, one window after another.
    fn paint_os_windows(&mut self, wgpu: &mut WgpuResources, frames: Vec<OsFrame>) {
        for mut frame in frames {
            let _span = tracing::info_span!(target: "regia_editor", "os_window_present").entered();
            if let Some(window) = self.os_windows.get(frame.at) {
                let jobs = self.egui_ctx.tessellate(
                    std::mem::take(&mut frame.output.shapes),
                    frame.output.pixels_per_point,
                );
                let screen = egui_wgpu::ScreenDescriptor {
                    size_in_pixels: frame.size,
                    pixels_per_point: frame.output.pixels_per_point,
                };
                let view = frame
                    .texture
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());
                let mut encoder = wgpu
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("panel window egui encoder"),
                    });
                wgpu.egui_renderer
                    .update_buffers(&wgpu.device, &wgpu.queue, &mut encoder, &jobs, &screen);
                {
                    let mut pass = encoder
                        .begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("panel window egui pass"),
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &view,
                                resolve_target: None,
                                ops: wgpu::Operations {
                                    load: wgpu::LoadOp::Clear(wgpu::Color {
                                        r: 0.02,
                                        g: 0.02,
                                        b: 0.025,
                                        a: 1.0,
                                    }),
                                    store: wgpu::StoreOp::Store,
                                },
                                depth_slice: None,
                            })],
                            depth_stencil_attachment: None,
                            occlusion_query_set: None,
                            timestamp_writes: None,
                            multiview_mask: None,
                        })
                        .forget_lifetime();
                    wgpu.egui_renderer.render(&mut pass, &jobs, &screen);
                }
                wgpu.queue.submit([encoder.finish()]);
                window.window.pre_present_notify();
                wgpu.queue.present(frame.texture);
            }
            for id in &frame.output.textures_delta.free {
                wgpu.egui_renderer.free_texture(id);
            }
            frame.output.textures_delta.clear();
        }
    }

    /// One event of a floated panel's OS window: input goes to its egui state (with the drop carry
    /// between windows), the close button docks its panels back, and the main frame redraws.
    fn on_os_window_event(&mut self, at: usize, event: &WindowEvent) {
        let frame = self.app.frame_count;
        let Some(window) = self.os_windows.get_mut(at) else {
            return;
        };
        let key = window.key();
        match event {
            WindowEvent::CloseRequested => {
                if let Some(serial) = self.app.shell.os_window_serial(window.viewport) {
                    regia_editor::os_windows::OsWindowRequests::push(
                        self.app.app.world_mut(),
                        regia_editor::os_windows::OsWindowRequest::DockWindow(serial),
                    );
                }
            }
            WindowEvent::Resized(size) => {
                if let Some(device) = self.render_device.as_ref() {
                    window.config.width = size.width.max(1);
                    window.config.height = size.height.max(1);
                    window.surface.configure(&device.device, &window.config);
                }
            }
            WindowEvent::RedrawRequested => {}
            _ => {
                let mut carried_in = None;
                match event {
                    WindowEvent::CursorMoved { device_id, position } => {
                        window.cursor = Some([position.x, position.y]);
                        if self.drop_carry.entered(&self.egui_ctx, key, frame) {
                            carried_in = Some(*device_id);
                        }
                    }
                    WindowEvent::MouseInput {
                        state: ElementState::Released,
                        ..
                    } => {
                        let size = window.window.inner_size();
                        if window.cursor.is_some_and(|pos| {
                            regia_editor::os_windows::outside_window(pos, [size.width, size.height])
                        }) {
                            self.drop_carry.release_outside(&self.egui_ctx, key, frame);
                        }
                    }
                    _ => {}
                }
                let _ = window.egui_winit.on_window_event(&window.window, event);
                if let Some(device_id) = carried_in {
                    let _ = window
                        .egui_winit
                        .on_window_event(&window.window, &carried_release(device_id));
                }
            }
        }
        // One frame drives every window.
        if let Some(main) = &self.window {
            main.request_redraw();
        }
    }
}

/// AppState helpers for PIE: request draining, the host mirror and the reload pause/resume.
impl AppState {
    /// A close was requested (window X or Escape): with unsaved level edits the shell prompts
    /// (`specs/19` § 7), otherwise the app exits cleanly (autosaves cleared — nothing to
    /// recover; a crashed session leaves them behind).
    fn request_close(&mut self) {
        // Every document with edits counts, not only the level stack (audit v033b).
        let dirty = !self.shell.unsaved_documents(self.app.world()).is_empty();
        if dirty {
            self.shell.prompt_close();
        } else {
            self.clean_exit();
        }
    }

    /// An agent asked the editor to quit: unlike a window close it never waits on a prompt.
    /// Unsaved level edits keep their autosaves so the next session can recover them.
    /// A dirty level is autosaved first (audit v033b), so the next launch recovers the newest edits.
    fn agent_quit(&mut self) {
        // The dock layout is saved into the project on every exit (ADR-2321).
        let _ = self.shell.save_layout_on_exit(self.app.world());
        let _clean = self.shell.prepare_agent_quit(self.app.world_mut());
        self.exit_requested = true;
    }

    /// Marks the exit and clears the autosaves (the user closed the editor deliberately), except
    /// the crash-recovery autosaves still on offer: they are the only copy of a crashed session.
    fn clean_exit(&mut self) {
        let _ = self.shell.save_layout_on_exit(self.app.world());
        self.shell.exit_cleanup_with(self.app.world_mut());
        self.exit_requested = true;
    }

    /// Acts on the unsaved-changes decision the shell collected during the last UI pass.
    fn apply_close_decision(&mut self) {
        match self.shell.take_close_decision() {
            Some(regia_editor::autosave::CloseDecision::SaveAndExit) => {
                // A clean level is not rewritten (an untitled one would land in assets/ for
                // nothing when only another document had edits).
                let level_dirty = self
                    .app
                    .world()
                    .get_resource::<UndoStack>()
                    .is_some_and(UndoStack::is_dirty);
                let toast = if level_dirty {
                    self.shell_save_level()
                } else {
                    "Saved".to_string()
                };
                if toast.starts_with("Saved") {
                    // The other open documents save too; a failure keeps the editor open.
                    match self.shell.save_other_documents(self.app.world_mut()) {
                        Ok(()) => self.clean_exit(),
                        Err(err) => self.shell.set_toast(format!("Save failed: {err}")),
                    }
                } else {
                    // The save failed (cancelled/IO): keep editing, surface why.
                    self.shell.set_toast(toast);
                }
            }
            Some(regia_editor::autosave::CloseDecision::DiscardAndExit) => self.clean_exit(),
            Some(regia_editor::autosave::CloseDecision::Cancel) => self.restart_normally = false,
            None => {}
        }
    }

    /// The shell's File → Save path, callable without the egui borrow (the close decision).
    fn shell_save_level(&mut self) -> String {
        // EditorShell::save_level is UI-private (it may open a dialog); drive the same level_io
        // path directly, reusing the shell only for the toast.
        let world = self.app.world_mut();
        let mut open = world
            .get_resource::<regia_editor::level_io::OpenLevel>()
            .cloned()
            .unwrap_or_else(|| regia_editor::level_io::new_level(world));
        let before = open.clone();
        // A level with no path yet: park it in the project's assets (unsaved-levels folder is
        // a later milestone); the dialog-free fallback keeps the close flow moving.
        if open.path.is_none()
            && let Some(root) = self.shell.project_root()
        {
            open.path = Some(regia_editor::level_io::free_untitled_level_path(
                &root.join("assets"),
            ));
        }
        let Some(path) = open.path.clone() else {
            return "Save canceled".into();
        };
        match regia_editor::level_io::save_level_and_terrain(world, &open, &path) {
            Ok(_) => {
                if let Some(root) = self.shell.project_root() {
                    regia_editor::autosave::clear_autosave_after_save(&root, &before, &path);
                }
                let mut open = open;
                open.path = Some(path.clone());
                world.insert_resource(open);
                regia_editor::terrain_tool::commit_terrain_save(world);
                // Clean only once the tile map is written too (ADR-7412).
                let tiles = regia_editor::mode2d::save_dirty_tilemap_and_mark_saved(world);
                format!("Saved {}{tiles}", path.display())
            }
            Err(err) => format!("Save failed: {err}"),
        }
    }

    /// Applies every queued PIE request (toolbar buttons, `editor.*` agent calls) once, in order.
    fn apply_pie_requests(&mut self) {
        let requests = std::mem::take(
            &mut self
                .app
                .world_mut()
                .get_resource_or_insert_with::<PieRequests>(Default::default)
                .queue,
        );
        if requests.is_empty() {
            return;
        }
        let game = self.current_game();
        for request in requests {
            let toast =
                regia_editor::pie::apply_request(self.app.world_mut(), &mut self.pie, request, game.clone());
            self.shell.set_toast(toast);
        }
    }

    /// The game library PIE registers, refreshed after every accepted hot reload.
    fn current_game(&self) -> Option<regia_editor::pie::CurrentGame> {
        self.app
            .world()
            .get_resource::<regia_editor::pie::CurrentGame>()
            .cloned()
    }

    /// Mirrors the host's current game library into the edit world (before the first PIE start
    /// and after every accepted reload).
    fn refresh_current_game(&mut self) {
        let current = self.host.as_ref().and_then(|host| {
            host.current_exports()
                .map(|(exports, generation)| regia_editor::pie::CurrentGame {
                    exports,
                    generation,
                    package: host.package_name().to_string(),
                })
        });
        if let Some(current) = current {
            self.app.world_mut().insert_resource(current);
        }
    }

    /// Pauses a running PIE session while a hot-reload swap runs (specs/20 § 3 step 3).
    fn pause_pie_for_reload(&mut self) -> Option<PlayMode> {
        self.pie.as_mut().map(PieSession::pause_for_reload)
    }

    /// Resumes the PIE session a reload paused (specs/20 § 3 step 10).
    fn resume_pie_after_reload(&mut self, previous: Option<PlayMode>) {
        if let (Some(session), Some(previous)) = (&mut self.pie, previous) {
            session.resume_after_reload(previous);
        }
    }
}

/// Packs the tightly-copied rows of a surface readback into RGBA8, swapping the R and B
/// channels per pixel when the surface stores them as BGRA — the DX12/Metal order that
/// `regia_render::screenshot` already converts (audit G finding 2).
fn pack_copied_rows(mapped: &[u8], width: u32, height: u32, padded_row: u32, bgra: bool) -> Vec<u8> {
    let row = (width * 4) as usize;
    let mut rgba = Vec::with_capacity(row * height as usize);
    for y in 0..height as usize {
        let start = y * padded_row as usize;
        rgba.extend_from_slice(&mapped[start..start + row]);
    }
    if bgra {
        for px in rgba.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
    }
    rgba
}

/// Maps the copy buffer and packs tightly-packed sRGB RGBA8 (converting BGRA surfaces).
/// Whether `format` stores pixels B,G,R,A (DX12 and Metal surfaces) rather than R,G,B,A:
/// the pack step swaps channels 0 and 2 for these (`regia_render::screenshot` converts the
/// same way). A pure function so the format set is unit-tested on every host (audit G
/// editor4-platform L6: a `matches!` dropped from the wiring must fail the suite anywhere).
fn is_bgra(format: wgpu::TextureFormat) -> bool {
    matches!(
        format,
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    )
}

fn read_back(
    device: &wgpu::Device,
    buffer: wgpu::Buffer,
    width: u32,
    height: u32,
    padded_row: u32,
    format: wgpu::TextureFormat,
) -> Result<regia_render::CapturedImage, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| format!("GPU poll failed: {e}"))?;
    rx.recv()
        .map_err(|_| "map callback dropped".to_string())?
        .map_err(|e| format!("readback failed: {e}"))?;
    let mapped = buffer
        .slice(..)
        .get_mapped_range()
        .map_err(|e| format!("map failed: {e:?}"))?;
    let bgra = is_bgra(format);
    let rgba = pack_copied_rows(&mapped, width, height, padded_row, bgra);
    Ok(regia_render::CapturedImage { width, height, rgba })
}

impl Drop for EditorState {
    fn drop(&mut self) {
        self.play_gpu = None;
        release_for_exit(&mut self.wgpu, &mut self.window, &mut self.egui_winit);
    }
}

/// The full teardown order: GPU surface, then the window, then the clipboard owner (`egui_winit`).
///
/// Regression (v0.2.4 retest, Wayland): `EventLoop::run_app` consumes the loop, so winit closes the
/// Wayland connection when it returns, while `EditorState` is still alive in `main`. The
/// `smithay-clipboard` worker inside `egui_winit::State` then destroyed its primary-selection
/// manager on a dead connection and the process died with SIGSEGV after `quit.request`. This
/// runs from `ApplicationHandler::exiting`, while the connection is still open; `Drop` repeats
/// it as a no-op fallback.
fn release_for_exit<G, W, C>(gpu: &mut Option<G>, window: &mut Option<W>, clipboard: &mut Option<C>) {
    release_surface_then_window(gpu, window);
    *clipboard = None;
}

/// Drops the GPU state, whose surface uses the window's native handle, and then the window.
///
/// Regression (unsafe audit 2026-10-21): `EditorState` declares `window` before `wgpu`, and
/// fields drop in declaration order, so quitting the editor destroyed the window first and
/// the surface then released a dead native handle. Since the audit of 2026-11-01 the surface holds
/// its own clone of the window (no raw handles, no `unsafe`), so it cannot outlive it either way.
fn release_surface_then_window<G, W>(gpu: &mut Option<G>, window: &mut Option<W>) {
    *gpu = None;
    *window = None;
}

impl ApplicationHandler for EditorState {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            self.init_wgpu(event_loop);
        }
    }

    /// Every exit ends here with the display connection still open: release the Wayland
    /// clipboard worker now, not in `main` after `run_app` has closed the connection.
    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        // An exit with the play window open never runs the close path that remembers its
        // geometry: close it through the state machine once (fixrev8 finding 3), then write
        // the preferences the close touched.
        if self.play_window.id().is_some() {
            let mut platform = WinitPlayWindow {
                event_loop,
                device: self.render_device.as_ref(),
                gpu: &mut self.play_gpu,
            };
            self.play_window
                .sync(self.app.app.world_mut(), None, &mut platform);
            regia_editor::editor_prefs::persist_prefs(self.app.app.world_mut());
        }
        // The view state's last changes (ADR-7275).
        regia_editor::view_state::flush(self.app.app.world_mut());
        self.play_gpu = None;
        // Floated panels' windows: each surface drops before its window (field order).
        self.os_windows.clear();
        self.render_device = None;
        release_for_exit(&mut self.wgpu, &mut self.window, &mut self.egui_winit);
    }

    /// Raw mouse motion goes to the play world while the play window holds the mouse (ADR-7272).
    fn device_event(&mut self, event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event
            && self.play_window.captured()
            && let Some(id) = self.play_window.id()
        {
            self.deliver_play_window_event(
                event_loop,
                id,
                PlayWindowEvent::MouseMotion {
                    dx: delta.0,
                    dy: delta.1,
                },
            );
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
        if self.observe_pending
            && let Some(window) = &self.window
        {
            self.observe_pending = false;
            observe_main_window(window, self.app.app.world_mut());
        }
        // A floated panel's OS window (ADR-7821).
        if let Some(at) = self.os_windows.iter().position(|w| w.window.id() == window_id) {
            self.on_os_window_event(at, &event);
            return;
        }
        // Another window's events (the play window's) never reach the editor's egui or shortcuts.
        if self
            .window
            .as_ref()
            .is_some_and(|window| window.id() != window_id)
        {
            self.on_play_window_event(event_loop, window_id, &event);
            return;
        }
        let Some(egui_winit) = &mut self.egui_winit else {
            return;
        };
        let Some(window) = &self.window else {
            return;
        };

        // A drag that left the main window carries its payload to the window it is dropped on;
        // one carried here from another window lands (ADR-7821).
        let mut carried_in = None;
        match &event {
            WindowEvent::CursorMoved { device_id, position } => {
                self.main_cursor = Some([position.x, position.y]);
                if self
                    .drop_carry
                    .entered(&self.egui_ctx, MAIN_WINDOW_KEY, self.app.frame_count)
                {
                    carried_in = Some(*device_id);
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                ..
            } => {
                let size = window.inner_size();
                if self.main_cursor.is_some_and(|pos| {
                    regia_editor::os_windows::outside_window(pos, [size.width, size.height])
                }) {
                    self.drop_carry
                        .release_outside(&self.egui_ctx, MAIN_WINDOW_KEY, self.app.frame_count);
                }
            }
            _ => {}
        }
        let response = egui_winit.on_window_event(window, &event);
        if let Some(device_id) = carried_in {
            let _ = egui_winit.on_window_event(window, &carried_release(device_id));
            window.request_redraw();
        }

        if response.consumed && !matches!(event, WindowEvent::CloseRequested) {
            return;
        }

        match event {
            WindowEvent::CloseRequested => self.app.request_close(),
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key: Key::Named(NamedKey::F11),
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } if self.egui_ctx.input(|i| i.modifiers.ctrl && i.modifiers.alt) => {
                self.app
                    .app
                    .world_mut()
                    .insert_resource(regia_editor::code_reload::CompileRequest(true));
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key: Key::Named(NamedKey::F1),
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } if self.egui_ctx.input(|i| {
                regia_editor::play_input::is_release_key(
                    true,
                    i.modifiers.shift,
                    i.modifiers.ctrl,
                    i.modifiers.alt,
                )
            }) =>
            {
                self.apply_play_capture(regia_editor::play_input::CaptureEvent::ReleaseKey);
            }
            WindowEvent::Focused(false) => {
                self.apply_play_capture(regia_editor::play_input::CaptureEvent::Lost);
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key: Key::Named(NamedKey::Escape),
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => {
                // Panels and an open drawer use Escape first. With a play session running,
                // Escape stops it (`specs/19` § 4); otherwise it asks to close like the window's
                // close button. Play stops before the full-screen viewport ends (ADR-7753).
                match regia_editor::workspaces::escape_action_full(
                    self.app.pie.is_some(),
                    regia_editor::drawers::drawer_open(self.app.app.world())
                        || self.egui_ctx.egui_wants_keyboard_input(),
                    regia_editor::workspaces::fullscreen_in(self.app.app.world()),
                ) {
                    regia_editor::shell::EscapeAction::Ignore => {}
                    regia_editor::shell::EscapeAction::StopPlay => self
                        .app
                        .app
                        .world_mut()
                        .get_resource_or_insert_with::<PieRequests>(Default::default)
                        .push(regia_editor::pie::PieRequest::Stop),
                    regia_editor::shell::EscapeAction::Close => self.app.request_close(),
                    regia_editor::shell::EscapeAction::LeaveFullscreen => {
                        regia_editor::workspaces::ViewRequests::push(
                            self.app.app.world_mut(),
                            regia_editor::workspaces::ViewRequest::LeaveFullscreen,
                        )
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                self.frame_driver.editor_frame_ran(std::time::Instant::now());
                // Hot reload: poll the trigger/watch (PIE is paused around a swap, specs/20
                // § 3 step 3). The swap happens in the host's staging world; a running PIE
                // session keeps its generation until the next Play.
                let pie_mode = self.app.pause_pie_for_reload();
                // The toolbar's Compile button / Ctrl+Alt+F11 (specs/19 § 5).
                let compile = self
                    .app
                    .app
                    .world_mut()
                    .get_resource_mut::<regia_editor::code_reload::CompileRequest>()
                    .is_some_and(|mut request| std::mem::take(&mut request.0));
                if compile
                    && let Some(report) = self.app.host.as_mut().and_then(|host| host.request_compile())
                {
                    self.app.shell.set_toast(report.toast.clone());
                }
                if let Some(report) = self.app.host.as_mut().and_then(|host| host.poll()) {
                    self.app.shell.set_toast(report.toast.clone());
                    if report.accepted {
                        self.app.refresh_current_game();
                    }
                }
                // The game build's task row (its Cancel, `editor.cancel_task`, ADR-9770).
                regia_editor::tasks::sync_game_build(
                    self.app.app.world_mut(),
                    self.app.host.as_ref().and_then(HotReloadHost::compile_task),
                );
                let code = regia_editor::code_reload::CodeReloadStatus {
                    game: self.app.host.as_ref().map(|host| host.package_name().to_string()),
                    state: self.app.host.as_ref().map(HotReloadHost::compile_state),
                    report_serial: self.app.host.as_ref().map_or(0, HotReloadHost::report_serial),
                };
                self.app.app.world_mut().insert_resource(code);
                regia_editor::code_reload::service_hot_reload_job(
                    self.app.app.world_mut(),
                    self.app.host.as_ref(),
                );
                self.app.resume_pie_after_reload(pie_mode);
                // PIE: apply queued requests (toolbar, agent surface), step the play world.
                self.app.apply_pie_requests();
                // PIE client worlds (ADR-2623): built before the play world's first frame, stepped in lockstep.
                regia_editor::pie_worlds::pump(self.app.app.world_mut(), self.app.pie.as_mut());
                if let Some(session) = self.app.pie.as_mut()
                    && let Some(panic) = session.frame()
                {
                    tracing::error!(target: "regia_editor", "stopping play after a game panic: {panic}");
                    if let Some(session) = self.app.pie.take() {
                        regia_editor::pie::stop_after_panic(self.app.app.world_mut(), session);
                    }
                    self.app
                        .shell
                        .set_toast(format!("play stopped by a panic: {panic}"));
                }
                regia_editor::pie::sync_status(self.app.app.world_mut(), self.app.pie.as_ref());
                // The separate play window follows play (opens, moves to a restarted world, closes).
                self.sync_play_window(event_loop);
                // Play input capture (specs/19 § 4): a click on the possessed play view grabs the
                // mouse; play ending releases it. The drained click feeds `frame_event`: while
                // the session plays in the separate window the editor viewport's click owns
                // nothing here — the play window holds the mouse (ADR-7272; fixrev8 finding 1).
                let clicked = self.app.pie.as_mut().is_some_and(|s| {
                    s.app
                        .world_mut()
                        .remove_resource::<regia_editor::play_input::CaptureClick>()
                        .is_some()
                }) | self
                    .app
                    .app
                    .world_mut()
                    .remove_resource::<regia_editor::play_input::CaptureClick>()
                    .is_some();
                if let Some(event) = regia_editor::play_input::frame_event(
                    self.app.pie.is_some(),
                    self.app.pie.as_ref().is_some_and(|s| s.options.window),
                    clicked,
                ) {
                    self.apply_play_capture(event);
                }
                // Several-client PIE: the in-memory net session follows play and steps with it.
                let paused = self.app.pie.as_ref().is_some_and(|s| s.mode == PlayMode::Paused);
                regia_editor::pie_net::sync_with_play(self.app.app.world_mut(), self.app.pie.is_some());
                regia_editor::pie_net::step_frame(self.app.app.world_mut(), paused);
                regia_editor::abilities_panel::pump(self.app.app.world_mut(), self.app.pie.as_mut());
                regia_editor::audio_mixer_panel::pump(self.app.app.world_mut(), self.app.pie.as_mut());
                regia_editor::xr_simulator::pump(self.app.app.world_mut(), self.app.pie.as_mut());
                regia_editor::tests_panel::pump(self.app.app.world_mut(), self.app.pie.as_mut());
                regia_editor::state_tree_panel::pump(self.app.app.world_mut(), self.app.pie.as_mut());
                regia_editor::debug_overlay_selected::pump(self.app.app.world_mut(), self.app.pie.as_mut());
                regia_editor::visual_log_panel::pump(self.app.app.world_mut(), self.app.pie.as_mut());
                regia_editor::bt_editor::pump(self.app.app.world_mut(), self.app.pie.as_mut());
                regia_editor::state_tree_editor::pump(self.app.app.world_mut(), self.app.pie.as_mut());
                regia_editor::logic_graph_editor::pump(self.app.app.world_mut(), self.app.pie.as_mut());
                regia_editor::smart_object_panel::pump(self.app.app.world_mut(), self.app.pie.as_mut());
                regia_editor::camera_panel::pump(self.app.app.world_mut(), self.app.pie.as_mut());
                // The Match strip's queued controls and the live match view (ADR-8738).
                regia_editor::match_rules_play::pump(self.app.app.world_mut(), self.app.pie.as_mut());
                regia_editor::vehicle_tuning::pump(self.app.app.world_mut(), self.app.pie.as_mut());
                // Live link: edit-world edits into the running session (ADR-7833).
                regia_editor::live_link::pump(
                    self.app.app.world_mut(),
                    self.app.pie.as_mut().map(|session| session.app.world_mut()),
                );
                // The edit world advances (agent requests, level lifecycle, propagation).
                self.app.app.update();
                let restart = self
                    .app
                    .app
                    .world_mut()
                    .get_resource_mut::<regia_editor::safe_mode::SafeMode>()
                    .is_some_and(|mut state| std::mem::take(&mut state.restart_requested));
                if restart {
                    self.app.restart_normally = true;
                    self.app.request_close();
                }
                // `quit.request` (or an `AppExit`) in the edit world: exit like a close request.
                if self
                    .app
                    .app
                    .world_mut()
                    .get_resource_mut::<regia_editor::quit::EditorQuit>()
                    .is_some_and(|mut quit| quit.take())
                {
                    self.app.agent_quit();
                }
                // `agent.shutdown` (ADR-2412) saved the edit world's documents; the shell saves its own,
                // then the editor quits like `quit.request`.
                if self
                    .app
                    .app
                    .world_mut()
                    .remove_resource::<regia_editor::headless::ShutdownRequest>()
                    .is_some()
                {
                    match self.app.shell.save_other_documents(self.app.app.world_mut()) {
                        Ok(()) => self.app.agent_quit(),
                        Err(err) => self
                            .app
                            .shell
                            .set_toast(format!("Shutdown canceled, save failed: {err}")),
                    }
                }
                // The floated panels' OS windows the last root pass listed open or close first;
                // `render` runs their passes after the root pass and draws them (ADR-7821).
                self.sync_os_windows(event_loop);
                self.render();
                self.render_play_window(event_loop);

                self.app.frame_count += 1;
                if let Some(limit) = self.app.exit_after
                    && self.app.frame_count >= limit
                {
                    event_loop.exit();
                }
            }
            WindowEvent::Resized(_) => {
                // The next frame re-syncs egui's screen to the new surface size; ask for it
                // now so the window never shows a stale layout (round-2 fix).
                if let Some(window) = &self.window {
                    window.request_redraw();
                    observe_main_window(window, self.app.app.world_mut());
                }
            }
            // The window state saved on exit follows the window (ADR-7753).
            WindowEvent::Moved(_) => {
                if let Some(window) = &self.window {
                    observe_main_window(window, self.app.app.world_mut());
                }
            }
            _ => {}
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _wake: ()) {
        // A SpaceMouse sample or hot-plug (the device thread's waker): draw a frame now.
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // The shell decided the app should exit (close decision or a clean close request).
        if self.app.exit_requested {
            event_loop.exit();
            return;
        }
        let Some(window) = &self.window else {
            return;
        };
        // Repaint cadence: full rate while the viewport is realtime or PIE is playing;
        // otherwise a 1 Hz keep-alive (cheap egui frames, agent edits become visible within a
        // second, idle CPU stays low — specs/19 § 8 M10). The redraw is only requested when the
        // deadline passed, so a quiet editor blocks in the event loop instead of spinning.
        let playing = self
            .app
            .pie
            .as_ref()
            .is_some_and(|session| session.mode == PlayMode::Playing);
        let realtime = self
            .app
            .app
            .world()
            .get_resource::<ViewportSettings>()
            .is_some_and(|s| s.realtime);
        let now = std::time::Instant::now();
        // The keep-alive is 1 Hz: the offscreen forward pass rebuilds its pipelines per call
        // (M5's readback path), so a 4 Hz wake would still cost ~15% CPU. User interaction and
        // egui repaint requests still draw at full rate.
        let due = editor_redraw_due(
            self.occluded,
            playing || realtime,
            self.egui_ctx.has_requested_repaint()
                // A floated panel's OS window asks through the main frame (ADR-7821).
                || self
                    .os_windows
                    .iter()
                    .any(|w| self.egui_ctx.has_requested_repaint_for(&w.viewport)),
            now.duration_since(self.last_redraw),
        );
        if due {
            self.last_redraw = now;
            window.request_redraw();
            if let Some(play) = &self.play_gpu {
                play.window.request_redraw();
            }
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(now + std::time::Duration::from_secs(1)));
    }
}

struct Args {
    exit_after: Option<u64>,
    game: Option<String>,
    project: Option<String>,
    /// `--headless`: no window, offscreen views (ADR-2412).
    headless: bool,
    /// `--no-gpu` (headless only): no device; view methods answer `editor_headless`.
    no_gpu: bool,
    /// `--exit-on-stdin-eof` (headless only): end, saving, when stdin reaches its end (ADR-2821).
    exit_on_stdin_eof: bool,
    /// `--safe-mode`: suppress project code and automatic restores.
    safe_mode: bool,
    /// Explicit normal restart after the recovery prompt.
    normal_mode: bool,
}

fn parse_args() -> Args {
    let args: Vec<String> = std::env::args().collect();
    let mut exit_after = None;
    let mut game = None;
    let mut project = None;
    let mut open_arg = None;
    let mut headless = false;
    let mut no_gpu = false;
    let mut exit_on_stdin_eof = false;
    let mut safe_mode = false;
    let mut normal_mode = false;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--safe-mode" {
            safe_mode = true;
            i += 1;
            continue;
        }
        if args[i] == "--normal-mode" {
            normal_mode = true;
            i += 1;
            continue;
        }
        if args[i] == "--headless" {
            headless = true;
            i += 1;
            continue;
        }
        if args[i] == "--no-gpu" {
            no_gpu = true;
            i += 1;
            continue;
        }
        if args[i] == "--exit-on-stdin-eof" {
            exit_on_stdin_eof = true;
            i += 1;
            continue;
        }
        if args[i] == "--exit-after-frames" && i + 1 < args.len() {
            exit_after = args[i + 1].parse::<u64>().ok();
            i += 2;
            continue;
        }
        if args[i] == "--game" && i + 1 < args.len() {
            game = Some(args[i + 1].clone());
            i += 2;
            continue;
        }
        if args[i] == "--project" && i + 1 < args.len() {
            project = Some(args[i + 1].clone());
            i += 2;
            continue;
        }
        // A bare path from a desktop launcher (`Exec=... %F`, ADR-20000); `--project` wins.
        if open_arg.is_none() && !args[i].starts_with('-') {
            open_arg = regia_editor::app_icon::project_from_open_arg(std::path::Path::new(&args[i]))
                .map(|p| p.display().to_string());
        }
        i += 1;
    }
    let project = project.or(open_arg);
    Args {
        exit_after,
        game,
        project,
        headless,
        no_gpu,
        exit_on_stdin_eof,
        safe_mode,
        normal_mode,
    }
}

/// The edit world: minimal app + scene + agent protocol, with the editor's resources and the
/// `editor.*` extension methods installed.
fn edit_app(
    project: Option<&std::path::Path>,
    spacemouse_waker: Option<regia_editor::spacemouse::SpaceMouseWaker>,
    safe_mode: regia_editor::safe_mode::SafeMode,
) -> (App, Entity) {
    let mut app = App::new();
    let safe = safe_mode.enabled;
    app.insert_resource(safe_mode);
    app.add_plugins(MinimalPlugins);
    // The editor has a minimal app rather than DefaultPlugins. Share the logging profiler
    // with both the Profiler panel and the existing profiler.* agent methods.
    let profiler = regia::frame_profiler();
    app.insert_resource(regia::Profiler(profiler.clone()))
        .insert_resource(regia::telemetry::SpanProfiler(profiler))
        .add_systems(Last, |profiler: Res<regia::Profiler>| profiler.0.end_frame());
    // Native crash capture with the game defaults (Linux on, Windows opt-in). The monitor child
    // is started by `run_crash_monitor_if_requested` at the top of `main` (ADR-3931).
    app.add_plugins(regia_framework::feedback::FeedbackPlugin {
        config: regia_editor::crash_reports::editor_feedback_config(),
    });
    app.add_plugins(regia_scene::ScenePlugin);
    // SpaceMouse viewport navigation (specs/29): device samples wake the event loop.
    if let Some(waker) = spacemouse_waker {
        app.insert_resource(waker);
    }
    app.add_plugins(regia_editor::spacemouse::SpaceMousePlugin::default());
    // Live asset reload (specs/12 § 8): the asset server's messages and status in the edit
    // world, fed by the editor's live cooker (started when a project opens), engine shader
    // swaps for the viewport renderers.
    app.insert_resource(regia_asset::ExternalReloadSource)
        .insert_resource(regia_editor::asset_watch::LiveAssetReload::from_env())
        .add_plugins(regia_asset::AssetPlugin {
            settings: regia_asset::AssetServerSettings {
                project_root: project
                    .map_or_else(regia_asset::detect_project_root, std::path::Path::to_path_buf),
                platform: regia_asset::current_platform(),
                content_dir: None,
            },
        })
        .init_resource::<regia_render::shader_reload::ShaderReloadJobs>()
        .add_systems(Update, regia_render::shader_reload::reload_engine_shaders);
    if let Some(project) = project {
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(project.to_path_buf()),
        });
    }
    if !safe {
        app.add_plugins(regia_agent::AgentPlugin);
    }
    // Lua script mods: the host, the `lua.*` agent methods and the script-component types
    // (ADR-3356). `mods/` of the open project loads below, once a project is open.
    if !safe {
        regia_editor::lua_mods::install(&mut app);
    }
    // `procgen.plant_build` writes through the asset pipeline (spec 68 § 6.3).
    app.insert_resource(regia_agent::PlantExportHook {
        write: regia_asset_pipeline::plant_export::host_write,
        load_mesh: regia_asset_pipeline::plant_export::host_load_mesh,
    });
    // `import.gltf` and `import.obj` in `fuzz.targets` (ADR-2084).
    regia_editor::import_fuzz::install(app.world_mut());
    // `editor.prefs`, `editor.keymap`, `editor.layout` (ADR-2321).
    regia_editor::baseline_fuzz::install(app.world_mut());
    // `editor.view_state`, `editor.autosave_index`, `editor.tool_manifest` (w11 item 5).
    regia_editor::state_fuzz::install(app.world_mut());
    // Per-user preferences, keymap and default layout (ADR-2321); the shell reads them on its
    // first frame and writes them when they change.
    app.world_mut()
        .insert_resource(regia_editor::editor_prefs::EditorConfigPaths::user());
    regia_editor::editor_prefs::load_prefs(app.world_mut());
    regia_editor::editor_keymap::load_keymap(app.world_mut());
    // Extensions: `editor` audience, dynamic libraries behind the ABI stamp (specs/57).
    if !safe {
        app.add_plugins(regia_editor::extension::EditorExtensionPlugin {
            project: project.map(std::path::Path::to_path_buf),
            target_dir: None,
            engine_dir: regia_editor::extension::host::default_engine_dir(),
            linked: Vec::new(),
        });
    }
    // `quit.request` has no platform runner behind it here: the editor turns it into an exit.
    regia_editor::quit::install(&mut app);
    // The game's renderer and its VFX layer (placed decals, particles): the viewport renders
    // through this `RuntimeRenderer`, so whatever draws in the game draws here.
    app.init_resource::<regia_render::RuntimeRenderer>()
        .init_resource::<regia_render::ExtractedFrame>()
        .add_plugins((regia_vfx::VfxPlugin, regia_vfx::VfxRuntimePlugin));

    // Editor resources the panels and the agent surface share.
    let window_marker = app.world_mut().spawn(regia_platform::PrimaryWindow).id();
    app.init_resource::<regia_render::ScreenshotRequests>()
        .init_resource::<UndoStack>()
        .init_resource::<regia_editor::selection::Selection>()
        .init_resource::<regia_editor::viewport::EditorCamera>()
        .init_resource::<regia_editor::viewport::GizmoState>()
        .init_resource::<ViewportSettings>()
        .init_resource::<regia_editor::pie::PieRequests>()
        .init_resource::<regia_editor::pie::PieStatus>()
        .init_resource::<regia_editor::pie_net::PieNet>()
        .init_resource::<regia_editor::prefab_edit::PrefabEditMode>()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: project.map(std::path::Path::to_path_buf),
        });
    let open = regia_editor::level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    // Camera rig components are level content (`regia_framework::camera`): register the
    // types so the inspector and `scene.add_component` can author them. Their *systems* stay
    // out of the edit world (registering types runs no schedules).
    app.register_type::<regia_framework::camera::CameraRig>()
        .register_type::<regia_framework::camera::CameraMode>()
        .register_type::<regia_framework::camera::ThirdPerson>()
        .register_type::<regia_framework::camera::Overhead>()
        .register_type::<regia_framework::camera::FirstPerson>()
        .register_type::<regia_framework::camera::Free>()
        .register_type::<regia_framework::camera::ChaseCamera>()
        // Greybox shapes (Create ▸ Shape, `scene.spawn_primitive`) are level content too.
        .register_type::<regia_render::Primitive>()
        // The selection-root marker is saved with the level (ADR-7769).
        .register_type::<regia_editor::selection::EditorSelectionRoot>()
        .register_type::<regia_ai::smart_object::SmartObject>()
        .register_type::<regia_ai::smart_object::SmartObjectSlot>()
        // Logic authored on prefab and level entities (P-165, ADR-8566).
        .register_type::<regia_ai::logic::LogicGraphRef>()
        .register_type::<regia_ai::logic::LogicLocalOverride>()
        .register_type::<regia_ai::logic::LogicOverrideValue>()
        .init_resource::<regia_editor::state_tree_panel::StateTreePanel>()
        .init_resource::<regia_editor::smart_object_panel::SmartObjectPanel>()
        .init_resource::<regia_editor::camera_panel::CameraPanel>();
    // The Cameras panel's page: the registry and hot reload of `.camera.ron`, no driver. The edit
    // world has no game code, so it cannot know a game's director hook names.
    app.add_plugins(regia_framework::camera::load::CameraAssetsPlugin);
    // The open level's lightmap and its receivers, as in a game (ADR-2635).
    regia_editor::lightmap_panel::install_level_lightmaps(&mut app);
    app.world_mut()
        .resource_mut::<regia_framework::camera::director::CameraHooks>()
        .accept_any_name();
    // `--project` opens the folder and, when Regia.toml names one, the startup level.
    // A missing manifest keeps the empty project inserted above (the browser can replace it).
    if let Some(project) = project {
        match regia_editor::project::open_project(app.world_mut(), project) {
            Ok(opened) => {
                if let Some(warning) = opened.warning {
                    tracing::warn!(target: "regia_editor", "{warning}");
                }
            }
            Err(err) => tracing::warn!(target: "regia_editor", "did not open project: {err}"),
        }
        // The project's Lua mods (ADR-3356): declarations for the inspector, hot reload on save.
        if !safe {
            regia_editor::lua_mods::load_project_mods(app.world_mut(), project);
        }
    }
    // Procedural generators (spec 62) under the editor's staleness rules; the procedural graph
    // panel (spec 67) edits their graphs. The graph source follows the open project.
    app.insert_resource(regia_procgen::world::ProcGenSettings {
        editor: true,
        source: regia_procgen::world::ProcGraphSource::Dir(
            project.map_or_else(|| std::path::PathBuf::from("."), std::path::Path::to_path_buf),
        ),
        terrain_edits: project.is_some_and(regia_editor::project::procgen_terrain_edits),
        ..regia_procgen::world::ProcGenSettings::default()
    });
    app.add_plugins(regia_procgen::ProcGenPlugin);
    agent_ext::install(app.world_mut());
    // `editor.headless_status`, `editor.open_level`, `agent.shutdown`, `gen.*` (ADR-2412).
    regia_editor::headless::install(app.world_mut());
    // Editor tools (ADR-1523): reap exits, hand work to polls, expire calls, autostart on open.
    if !safe {
        app.add_systems(Last, regia_editor::tools::tick_tools);
        // Hosted agent sessions run external commands: off in safe mode like the other code hosts.
        app.add_systems(Last, regia_editor::agent_sessions::tick);
    }
    // The view state per project: restore on level open, record, write (ADR-7275).
    if !safe {
        app.add_systems(Last, regia_editor::view_state::tick);
    }
    // The save trigger of the asset validators (spec 32 § 4.5, ADR-2755): results arrive after the save.
    app.add_systems(Last, regia_editor::save_validation::tick_save_validation_system);
    // Editor worker jobs end on the editor thread: Import settings Apply, navigation preview (ADR-7664).
    app.add_systems(Last, regia_editor::editor_jobs::tick_system);
    // Revision-control status: a background `git status` when a change notification moved its key (ADR-7837).
    app.add_systems(Last, regia_editor::vcs::tick_vcs_status_system);

    // Start the agent server (AgentPlugin::finish) and run one frame so the world settles.
    app.finish_plugins();
    app.update();
    // Classify bookkeeping entities (the window marker, protocol spawns) as editor-internal:
    // hidden from the outliner and never selectable. User content (name/guid/transform) is
    // never affected; `frame_effects` repeats the sweep each frame.
    regia_editor::visibility::mark_internal_entities(app.world_mut());
    (app, window_marker)
}

/// Without `--game`, runs the editor on its own edit world. With `--game <crate>`, additionally
/// hosts the game dylib for hot reload.
fn main() {
    // First: the crash-monitor child exits here. Everywhere else this returns at once (ADR-3931).
    regia_framework::feedback::run_crash_monitor_if_requested();
    let mut args = parse_args();
    let _log = regia_editor::engine::core::log::init(regia_editor::engine::core::log::LogConfig {
        profiler: Some(regia::frame_profiler()),
        ..Default::default()
    });
    // Before the window: the same panic hook games use, aimed at this editor's log folder.
    regia_editor::crash_reports::install(regia_editor::crash_reports::default_log_dir());

    let (startup, repeated_failure) = match regia_editor::safe_mode::StartupAttempt::begin(
        &regia_editor::editor_prefs::user_config_dir(),
    ) {
        Ok((attempt, repeated)) => (Some(attempt), repeated && !args.normal_mode),
        Err(error) => {
            tracing::warn!(target: "regia_editor", %error, "startup attempt tracking unavailable");
            (None, false)
        }
    };
    args.safe_mode |= repeated_failure;
    let project = args.project.as_deref().map(std::path::Path::new);
    if args.headless {
        // A headless safe run deliberately starts no agent listener; explicit frame/EOF exit still works.
        let status = run_headless(&args, project, startup);
        regia_platform::finish_exit(status, || {
            if let Ok(log) = &_log {
                log.flush();
            }
        });
        // `finish_exit` returns once the GPU workers stopped; a failed save still ends non-zero.
        if status != 0 {
            std::process::exit(status);
        }
        return;
    }
    let event_loop = EventLoop::new().unwrap_or_else(|e| {
        eprintln!("regia-editor: failed to create event loop: {e}");
        std::process::exit(1);
    });
    // A SpaceMouse sample arrives on a reader thread; wake the (otherwise 1 Hz) idle loop.
    let proxy = event_loop.create_proxy();
    let waker = regia_editor::spacemouse::SpaceMouseWaker::new(move || {
        let _ = proxy.send_event(());
    });
    // An agent request wakes it too (ADR-7393): the inbox is read in a frame, so an editor idling
    // with realtime off answered every call only at the next 1 Hz keep-alive.
    regia_agent::server::set_app_wake(Some(Arc::clone(&waker.0)));
    let (mut app, window_marker) = edit_app(
        project,
        Some(waker),
        regia_editor::safe_mode::SafeMode::new(args.safe_mode, repeated_failure),
    );
    if let Some(startup) = startup {
        app.insert_resource(startup);
    }
    let mut host = None;
    if !args.safe_mode
        && let Some(game) = &args.game
    {
        match HotReloadConfig::discover(std::path::Path::new(game)) {
            Ok(config) => {
                let mut loaded = HotReloadHost::new(config);
                // M11: the game runs inside play-in-editor (`specs/19` § 4), not in the host's
                // private world — that world is the reload staging world from here on.
                loaded.set_playing(false);
                let _ = loaded.reload();
                host = Some(loaded);
            }
            Err(error) => tracing::error!(target: "regia_editor", "{error}"),
        }
    }

    let mut shell = EditorShell::new(None, project.map(std::path::Path::to_path_buf));
    shell.set_pending_reports(regia_editor::crash_reports::list());
    let mut app_state = AppState {
        shell,
        app,
        frame_count: 0,
        exit_after: args.exit_after,
        host,
        pie: None,
        window_marker,
        exit_requested: false,
        restart_normally: false,
    };
    // A game crate is open: publish its library so `editor.play` registers it into PIE.
    app_state.refresh_current_game();

    event_loop.set_control_flow(ControlFlow::Poll);

    let mut editor_state = EditorState::new(app_state);
    event_loop.run_app(&mut editor_state).unwrap_or_else(|e| {
        eprintln!("regia-editor: event loop error: {e}");
        std::process::exit(1);
    });
    let restart = editor_state.app.restart_normally;
    let startup = editor_state
        .app
        .app
        .world_mut()
        .remove_resource::<regia_editor::safe_mode::StartupAttempt>();
    end_of_process(editor_state, || {
        regia_editor::safe_mode::clean_exit(startup);
        if restart {
            let result = std::env::current_exe().and_then(|exe| {
                std::process::Command::new(exe)
                    .args(regia_editor::safe_mode::normal_arguments(
                        std::env::args_os().skip(1),
                    ))
                    .spawn()
            });
            if let Err(error) = result {
                eprintln!("regia-editor: normal restart failed: {error}");
            }
        }
        regia_platform::finish_exit(0, || {
            if let Ok(log) = &_log {
                log.flush();
            }
        });
    });
}

/// `--headless` (ADR-2412): the edit world and the agent server without a window or an event
/// loop, until `agent.shutdown`, `quit.request` or a signal. Returns the exit status; the app, its
/// renderers and the game host are dropped before it returns, so the caller can wait for the GPU
/// workers (ADR-287).
fn run_headless(
    args: &Args,
    project: Option<&std::path::Path>,
    startup: Option<regia_editor::safe_mode::StartupAttempt>,
) -> i32 {
    let (mut app, window_marker) = edit_app(
        project,
        None,
        regia_editor::safe_mode::SafeMode::new(args.safe_mode, false),
    );
    regia_editor::headless::prepare(app.world_mut(), !args.no_gpu);
    let mut host = None;
    if !args.safe_mode
        && let Some(game) = &args.game
    {
        match HotReloadConfig::discover(std::path::Path::new(game)) {
            Ok(config) => {
                let mut loaded = HotReloadHost::new(config);
                loaded.set_playing(false);
                let _ = loaded.reload();
                host = Some(loaded);
            }
            Err(error) => tracing::error!(target: "regia_editor", "{error}"),
        }
    }
    let options = regia_editor::headless::HeadlessOptions {
        gpu: !args.no_gpu,
        exit_after: args.exit_after,
        exit_on_stdin_eof: args.exit_on_stdin_eof,
        ..regia_editor::headless::HeadlessOptions::default()
    };
    let status = regia_editor::headless::run(app, window_marker, host, &options);
    if status == 0 {
        regia_editor::safe_mode::clean_exit(startup);
    }
    status
}

/// The end of the editor's process: `state` owns the app, its renderers and the game host, so it
/// goes first; `finish` ([`regia_platform::finish_exit`]) then waits for the GPU worker threads
/// they stopped. This binary has its own event loop, so it does what `regia::run_with` does for a
/// game: `main` must not return into the exit handlers with a worker inside the driver (ADR-287).
fn end_of_process<S>(state: S, finish: impl FnOnce()) {
    drop(state);
    finish();
}

// The event-loop proxy type is unused but referenced in docs; keep the import list stable.
#[allow(dead_code)]
type UnusedProxy = EventLoopProxy<()>;

/// The open project's saved window placement and the monitors it indexes (ADR-7753).
fn saved_placement(
    event_loop: &ActiveEventLoop,
    app: &AppState,
) -> Option<(
    regia_editor::window_state::Placement,
    Vec<winit::monitor::MonitorHandle>,
)> {
    let root = app.shell.project_root()?;
    let config = app
        .app
        .world()
        .get_resource::<regia_editor::editor_prefs::EditorConfigPaths>()?
        .dir
        .clone()?;
    let record = regia_editor::window_state::read_record(&config, &root)?;
    let monitors: Vec<winit::monitor::MonitorHandle> = event_loop.available_monitors().collect();
    let primary = event_loop.primary_monitor();
    let infos: Vec<regia_editor::window_state::MonitorInfo> = monitors
        .iter()
        .map(|m| regia_editor::window_state::MonitorInfo {
            name: m.name(),
            position: (m.position().x, m.position().y),
            size: (m.size().width, m.size().height),
            primary: primary.as_ref() == Some(m),
        })
        .collect();
    Some((regia_editor::window_state::place(&record, &infos), monitors))
}

/// Records the main window's monitor, position, size and full-screen state for the exit's window
/// state save (ADR-7753).
fn observe_main_window(window: &Window, world: &mut World) {
    let size = window.inner_size();
    // A minimized window reports 0x0 (and -32000 positions on Windows): keep the last real
    // placement instead (review sprint-v06-rev-late-k #28).
    if window.is_minimized() == Some(true) || size.width == 0 || size.height == 0 {
        return;
    }
    let observed = regia_editor::window_state::ObservedWindow {
        monitor: window.current_monitor().and_then(|m| m.name()),
        position: window.outer_position().ok().map(|p| (p.x, p.y)),
        size: (size.width, size.height),
        fullscreen: window.fullscreen().is_some(),
        maximized: window.is_maximized(),
    };
    if world.get_resource::<regia_editor::window_state::ObservedWindow>() != Some(&observed) {
        world.insert_resource(observed);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Audit G finding 2: surfaces on DX12 (Windows) and Metal (macOS) are usually
    /// `Bgra8UnormSrgb`; the copy buffer then holds B,G,R,A per pixel and the pack step must
    /// swap channels 0 and 2 or every editor screenshot and readback answers with red and
    /// blue swapped (`regia_render::screenshot` converts the same way).
    #[test]
    fn read_back_packs_bgra_as_rgba() {
        // Two 2-pixel rows with 8-byte padding (COPY_BYTES_PER_ROW_ALIGNMENT): BGRA source
        // pixels (B=10/50, G=20, R=30/60, A=40) on the first row.
        let mapped = [
            10u8, 20, 30, 40, 50, 20, 60, 40, // row 0: two BGRA pixels
            0, 0, 0, 0, 0, 0, 0, 0, // row 1: padding only
        ];
        let rgba = super::pack_copied_rows(&mapped, 2, 1, 8, true);
        assert_eq!(rgba, vec![30, 20, 10, 40, 60, 20, 50, 40], "BGRA swapped to RGBA");
        let identity = super::pack_copied_rows(&mapped, 2, 1, 8, false);
        assert_eq!(
            identity,
            vec![10, 20, 30, 40, 50, 20, 60, 40],
            "RGBA stays untouched"
        );
    }

    /// Audit G editor4-platform L6 (sprint-fixrev-08-k): the format→BGRA wiring is tested
    /// on every host, so dropping a `Bgra8*` variant (or flipping the bool) fails the suite
    /// on Linux too, not silently on Windows CI.
    #[test]
    fn is_bgra_covers_the_surface_formats() {
        assert!(super::is_bgra(wgpu::TextureFormat::Bgra8Unorm));
        assert!(super::is_bgra(wgpu::TextureFormat::Bgra8UnormSrgb));
        assert!(!super::is_bgra(wgpu::TextureFormat::Rgba8Unorm));
        assert!(!super::is_bgra(wgpu::TextureFormat::Rgba8UnormSrgb));
    }

    /// Appends its name to a shared log when dropped.
    struct Noted(&'static str, Rc<RefCell<Vec<&'static str>>>);

    impl Drop for Noted {
        fn drop(&mut self) {
            self.1.borrow_mut().push(self.0);
        }
    }

    /// The editor's shutdown order: the surface goes before the window it was made from, even
    /// though `EditorState` declares the window first (field order would drop it first).
    #[test]
    fn the_surface_is_released_before_the_window() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut gpu = Some(Noted("surface", Rc::clone(&log)));
        let mut window = Some(Noted("window", Rc::clone(&log)));
        super::release_surface_then_window(&mut gpu, &mut window);
        assert_eq!(*log.borrow(), ["surface", "window"]);
        assert!(gpu.is_none() && window.is_none());

        // Field order alone, for contrast: a struct shaped like `EditorState` drops `window` first.
        struct Fields {
            _window: Option<Noted>,
            _gpu: Option<Noted>,
        }
        log.borrow_mut().clear();
        drop(Fields {
            _window: Some(Noted("window", Rc::clone(&log))),
            _gpu: Some(Noted("surface", Rc::clone(&log))),
        });
        assert_eq!(*log.borrow(), ["window", "surface"]);
    }

    /// The clipboard owner goes last, and a second release (the `Drop` fallback) is a no-op.
    #[test]
    fn the_clipboard_owner_is_released_after_surface_and_window() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut gpu = Some(Noted("surface", Rc::clone(&log)));
        let mut window = Some(Noted("window", Rc::clone(&log)));
        let mut clipboard = Some(Noted("clipboard", Rc::clone(&log)));
        super::release_for_exit(&mut gpu, &mut window, &mut clipboard);
        assert_eq!(*log.borrow(), ["surface", "window", "clipboard"]);
        super::release_for_exit(&mut gpu, &mut window, &mut clipboard);
        assert_eq!(log.borrow().len(), 3);
    }

    /// The end of the process waits for the GPU workers only once what stops them is gone.
    #[test]
    fn the_end_of_the_process_drops_the_app_before_it_waits_for_the_workers() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let state = Noted("app", Rc::clone(&log));
        let finish_log = Rc::clone(&log);
        super::end_of_process(state, move || finish_log.borrow_mut().push("finish"));
        assert_eq!(*log.borrow(), ["app", "finish"]);
    }

    /// M5, 2026-11-12, console locked (ADR-7335): every acquire was refused as occluded, the editor
    /// drew 0 frames and `seeing.screenshot` timed out. Occluded and timed-out acquires now draw the
    /// frame into the stand-in target; lost and invalid surfaces still skip it.
    #[test]
    fn occluded_and_timed_out_acquires_still_draw_the_frame() {
        use wgpu::CurrentSurfaceTexture as C;
        assert!(super::draws_into_stand_in(&C::Occluded));
        assert!(super::draws_into_stand_in(&C::Timeout));
        for skipped in [C::Lost, C::Validation, C::Outdated] {
            assert!(!super::draws_into_stand_in(&skipped));
        }
    }

    #[test]
    fn an_occluded_editor_redraws_at_the_keep_alive_rate_even_in_realtime() {
        use std::time::Duration;
        let soon = Duration::from_millis(10);
        assert!(
            super::editor_redraw_due(false, true, false, soon),
            "visible realtime: full rate"
        );
        assert!(
            !super::editor_redraw_due(true, true, false, soon),
            "occluded realtime: no spin"
        );
        assert!(
            super::editor_redraw_due(true, true, false, Duration::from_secs(1)),
            "keep-alive"
        );
        assert!(
            super::editor_redraw_due(true, false, true, soon),
            "egui repaint requests still draw"
        );
        assert!(
            !super::editor_redraw_due(false, false, false, soon),
            "idle: waits for the keep-alive"
        );
    }
}

#[cfg(test)]
mod safe_startup_tests {
    #[test]
    fn safe_edit_app_starts_without_code_hosts_or_agent_listeners() {
        let project = regia_test_utils::unique_temp_dir("safe-editor-app");
        std::fs::create_dir_all(&project).expect("project");
        let (app, _) = super::edit_app(
            Some(&project),
            None,
            regia_editor::safe_mode::SafeMode::new(true, false),
        );
        let world = app.world();
        assert!(regia_editor::safe_mode::enabled(world));
        assert!(!world.contains_resource::<regia_agent::AgentInbox>());
        assert!(!world.contains_resource::<regia_editor::extension::EditorExtensions>());
        assert!(!world.contains_resource::<regia_script_lua::LuaMods>());
        assert!(
            world
                .resource::<regia_editor::tools::ToolHost>()
                .endpoint()
                .is_none(),
            "safe startup also leaves the external-tool listener stopped"
        );
        drop(app);
        let _ = std::fs::remove_dir_all(project);
    }
}
