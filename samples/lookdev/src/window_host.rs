// SPDX-License-Identifier: MIT-0
//! Desktop window for `do27_chase --window`.
//!
//! The chase still builds the DO27 frame. This host only acquires a swapchain image, lets
//! the chase draw into it, and presents. Android is not built: the shell presents through
//! `regia::run`. Headless machines need `xvfb-run -a` (there is no display server on the
//! test box).
//!
//! Event pumping is winit 0.30's `EventLoopExtPumpEvents` so the chase keeps its own loop
//! instead of handing the thread to `EventLoop::run`. The swapchain starts from
//! [`wgpu::Surface::get_default_config`] and prefers an sRGB format with FIFO present.
//! [`wgpu::Surface::get_current_texture`] returns a [`wgpu::CurrentSurfaceTexture`]; a good or
//! suboptimal image is drawn and handed to [`wgpu::Queue::present`].

use std::sync::Arc;
use std::time::Duration;

use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::platform::pump_events::{EventLoopExtPumpEvents, PumpStatus};
use winit::window::{Window, WindowId};

use regia_render::RenderDevice;

use crate::LookdevError;
use crate::chase::FramePresent;

/// Title prefix `regia.` so the window lands on the silent dev workspace.
pub const WINDOW_TITLE: &str = "regia.do27_chase";

/// sRGB formats, in preference order, before whatever the surface lists first.
const SRGB_FORMATS: &[wgpu::TextureFormat] = &[
    wgpu::TextureFormat::Bgra8UnormSrgb,
    wgpu::TextureFormat::Rgba8UnormSrgb,
];

/// Picks an sRGB surface format when the surface lists one, otherwise the first format.
#[must_use]
pub fn choose_format(formats: &[wgpu::TextureFormat]) -> Option<wgpu::TextureFormat> {
    SRGB_FORMATS
        .iter()
        .copied()
        .find(|format| formats.contains(format))
        .or_else(|| formats.first().copied())
}

/// A winit window and its wgpu surface. The surface is created on the first presented frame,
/// once a [`RenderDevice`] exists. Fields drop in order: the surface before the window.
pub struct WindowHost {
    event_loop: EventLoop<()>,
    app: ChaseApp,
}

impl WindowHost {
    /// Opens a window of `width`×`height` and pumps until it exists.
    ///
    /// # Errors
    ///
    /// The event loop could not start, the window could not be created, or no `Resumed`
    /// event arrived (typical without a display).
    pub fn open(width: u32, height: u32) -> Result<Self, LookdevError> {
        let mut event_loop =
            EventLoop::new().map_err(|err| LookdevError::Config(format!("event loop: {err}")))?;
        let mut app = ChaseApp::new(width.max(1), height.max(1));
        for _ in 0..32 {
            if app.window.is_some() || app.close {
                break;
            }
            let status = event_loop.pump_app_events(Some(Duration::from_millis(50)), &mut app);
            if let PumpStatus::Exit(_) = status {
                app.close = true;
                break;
            }
        }
        if let Some(err) = app.open_error.take() {
            return Err(LookdevError::Config(format!("window: {err}")));
        }
        if app.window.is_none() {
            return Err(LookdevError::Config(
                "do27_chase --window: the window did not open. On a headless box run under xvfb-run -a"
                    .into(),
            ));
        }
        Ok(Self { event_loop, app })
    }

    fn pump(&mut self) -> Result<(), LookdevError> {
        let status = self
            .event_loop
            .pump_app_events(Some(Duration::ZERO), &mut self.app);
        if let PumpStatus::Exit(_) = status {
            self.app.close = true;
        }
        if let Some(err) = self.app.open_error.take() {
            return Err(LookdevError::Config(err));
        }
        Ok(())
    }
}

impl FramePresent for WindowHost {
    fn present_frame(
        &mut self,
        device: &RenderDevice,
        draw: &mut crate::chase::PresentDraw<'_>,
    ) -> Result<bool, LookdevError> {
        let _span = tracing::info_span!("do27_chase_window").entered();
        self.pump()?;
        if self.app.close {
            return Ok(false);
        }
        // Timeout, occlusion, or a caught validation error: skip the frame. The chase keeps going.
        let Some(frame) = self.app.acquire(device)? else {
            return Ok(true);
        };
        let format = self.app.format;
        let width = self.app.width.max(1);
        let height = self.app.height.max(1);
        let outcome = {
            let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
            draw(&view, format, width, height)
        };
        // Present even when the draw failed so the acquired image is not left outstanding.
        // The view is dropped with `outcome` above, before `present`.
        if let Some(window) = self.app.window.as_ref() {
            window.pre_present_notify();
        }
        device.queue.present(frame);
        outcome?;
        Ok(true)
    }
}

/// Window state. `surface` is declared before `window` so it drops first.
struct ChaseApp {
    surface: Option<wgpu::Surface<'static>>,
    config_width: u32,
    config_height: u32,
    format: wgpu::TextureFormat,
    window: Option<Arc<Window>>,
    close: bool,
    resized: bool,
    width: u32,
    height: u32,
    open_error: Option<String>,
}

impl ChaseApp {
    fn new(width: u32, height: u32) -> Self {
        Self {
            surface: None,
            config_width: 0,
            config_height: 0,
            format: wgpu::TextureFormat::Bgra8UnormSrgb,
            window: None,
            close: false,
            resized: false,
            width,
            height,
            open_error: None,
        }
    }

    fn ensure_surface(&mut self, device: &RenderDevice) -> Result<(), LookdevError> {
        let Some(window) = self.window.clone() else {
            return Err(LookdevError::Config(
                "window closed before a surface existed".into(),
            ));
        };
        if self.surface.is_none() {
            let surface = device
                .instance
                .create_surface(window)
                .map_err(|err| LookdevError::Config(format!("surface: {err}")))?;
            self.surface = Some(surface);
            self.resized = true;
        }
        if !self.resized && self.config_width == self.width && self.config_height == self.height {
            return Ok(());
        }
        let Some(surface) = self.surface.as_ref() else {
            return Err(LookdevError::Config("surface was not created".into()));
        };
        let caps = surface.get_capabilities(&device.adapter);
        let Some(format) = choose_format(&caps.formats) else {
            return Err(LookdevError::Config("the surface listed no formats".into()));
        };
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Fifo) {
            wgpu::PresentMode::Fifo
        } else {
            caps.present_modes
                .first()
                .copied()
                .unwrap_or(wgpu::PresentMode::Fifo)
        };
        let width = self.width.max(1);
        let height = self.height.max(1);
        let Some(mut config) = surface.get_default_config(&device.adapter, width, height) else {
            return Err(LookdevError::Config(
                "the adapter does not support this surface".into(),
            ));
        };
        config.format = format;
        config.present_mode = present_mode;
        config.width = width;
        config.height = height;
        surface.configure(&device.device, &config);
        self.format = format;
        self.config_width = width;
        self.config_height = height;
        self.resized = false;
        Ok(())
    }

    /// One acquire. `retry` reconfigures once on [`wgpu::CurrentSurfaceTexture::Outdated`] and
    /// recreates the surface once on [`wgpu::CurrentSurfaceTexture::Lost`].
    fn acquire(&mut self, device: &RenderDevice) -> Result<Option<wgpu::SurfaceTexture>, LookdevError> {
        self.acquire_once(device, true)
    }

    fn acquire_once(
        &mut self,
        device: &RenderDevice,
        retry: bool,
    ) -> Result<Option<wgpu::SurfaceTexture>, LookdevError> {
        self.ensure_surface(device)?;
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
        let current = {
            let Some(surface) = self.surface.as_ref() else {
                return Err(LookdevError::Config("surface was not created".into()));
            };
            surface.get_current_texture()
        };
        match current {
            wgpu::CurrentSurfaceTexture::Success(texture) => Ok(Some(texture)),
            wgpu::CurrentSurfaceTexture::Suboptimal(texture) => {
                self.resized = true;
                Ok(Some(texture))
            }
            wgpu::CurrentSurfaceTexture::Outdated if retry => {
                self.resized = true;
                self.acquire_once(device, false)
            }
            wgpu::CurrentSurfaceTexture::Lost if retry => {
                self.surface = None;
                self.resized = true;
                self.acquire_once(device, false)
            }
            wgpu::CurrentSurfaceTexture::Timeout
            | wgpu::CurrentSurfaceTexture::Occluded
            | wgpu::CurrentSurfaceTexture::Validation => Ok(None),
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => Err(
                LookdevError::Config("swapchain stayed outdated after a reconfigure".into()),
            ),
        }
    }
}

impl ApplicationHandler for ChaseApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title(WINDOW_TITLE)
            .with_inner_size(PhysicalSize::new(self.width, self.height));
        match event_loop.create_window(attributes) {
            Ok(window) => self.window = Some(Arc::new(window)),
            Err(err) => {
                self.open_error = Some(err.to_string());
                self.close = true;
            }
        }
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _window_id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => self.close = true,
            WindowEvent::Resized(size) if size.width > 0 && size.height > 0 => {
                self.width = size.width;
                self.height = size.height;
                self.resized = true;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_an_srgb_format_and_falls_back() {
        let listed = [
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        ];
        assert_eq!(choose_format(&listed), Some(wgpu::TextureFormat::Rgba8UnormSrgb));
        let bgra = [
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureFormat::Bgra8UnormSrgb,
        ];
        assert_eq!(choose_format(&bgra), Some(wgpu::TextureFormat::Bgra8UnormSrgb));
        let only = [wgpu::TextureFormat::Rgba8Unorm];
        assert_eq!(choose_format(&only), Some(wgpu::TextureFormat::Rgba8Unorm));
        assert_eq!(choose_format(&[]), None);
    }

    #[test]
    fn the_title_uses_the_regia_prefix() {
        assert!(WINDOW_TITLE.starts_with("regia."));
        assert_eq!(WINDOW_TITLE, "regia.do27_chase");
    }
}
