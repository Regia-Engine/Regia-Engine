// SPDX-License-Identifier: MPL-2.0
//! The canvas surface and device, on WebGPU or WebGL2.
//!
//! A canvas keeps the first context kind it hands out, so WebGPU's adapter and device are requested before
//! the canvas is touched: a refusal there leaves the canvas free for WebGL2. A failure after the surface
//! exists (the surface offers no format) leaves the canvas holding a WebGPU context; `auto` then reports
//! that failure instead of trying WebGL2 on a canvas that can no longer give it a context.

use web_sys::HtmlCanvasElement;

use crate::scene::FrameTarget;
use crate::seams::DrawOutcome;
use crate::{BackendChoice, WebError};

/// A device and a configured surface on a canvas.
pub struct Gpu {
    /// Which API this runs on: `webgpu` or `webgl2`.
    pub backend: &'static str,
    /// The adapter, for format and sample queries.
    pub adapter: wgpu::Adapter,
    /// The device.
    pub device: wgpu::Device,
    /// The queue.
    pub queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
}

fn unavailable(why: impl std::fmt::Display) -> WebError {
    WebError::new("backend_unavailable", why.to_string())
}

fn descriptor(backends: wgpu::Backends) -> wgpu::InstanceDescriptor {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = backends;
    desc
}

fn options<'a>(surface: Option<&'a wgpu::Surface<'static>>) -> wgpu::RequestAdapterOptions<'a, 'static> {
    wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        force_fallback_adapter: false,
        compatible_surface: surface,
        apply_limit_buckets: false,
    }
}

/// A failed WebGPU attempt, and whether it had already taken the canvas.
struct WebgpuRefusal {
    error: WebError,
    took_canvas: bool,
}

async fn on_webgpu(canvas: &HtmlCanvasElement, size: [u32; 2]) -> Result<Gpu, WebgpuRefusal> {
    let before = |error: WebError| WebgpuRefusal {
        error,
        took_canvas: false,
    };
    if !wgpu::util::is_browser_webgpu_supported().await {
        return Err(before(unavailable("this browser has no WebGPU adapter")));
    }
    let instance = wgpu::Instance::new(descriptor(wgpu::Backends::BROWSER_WEBGPU));
    let adapter = instance
        .request_adapter(&options(None))
        .await
        .map_err(|e| before(unavailable(e)))?;
    let (device, queue) = regia_viewer::gpu::request_viewer_device(&adapter)
        .await
        .map_err(|e| before(unavailable(e)))?;
    // From here on the canvas holds a WebGPU context, whatever happens next.
    let surface = instance
        .create_surface(wgpu::SurfaceTarget::Canvas(canvas.clone()))
        .map_err(|e| WebgpuRefusal {
            error: unavailable(e),
            took_canvas: true,
        })?;
    finish("webgpu", adapter, device, queue, surface, size).map_err(|error| WebgpuRefusal {
        error,
        took_canvas: true,
    })
}

async fn on_webgl2(canvas: &HtmlCanvasElement, size: [u32; 2]) -> Result<Gpu, WebError> {
    let instance = wgpu::Instance::new(descriptor(wgpu::Backends::GL));
    let surface = instance
        .create_surface(wgpu::SurfaceTarget::Canvas(canvas.clone()))
        .map_err(unavailable)?;
    let adapter = instance
        .request_adapter(&options(Some(&surface)))
        .await
        .map_err(unavailable)?;
    let (device, queue) = regia_viewer::gpu::request_viewer_device(&adapter)
        .await
        .map_err(unavailable)?;
    finish("webgl2", adapter, device, queue, surface, size)
}

fn finish(
    backend: &'static str,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    size: [u32; 2],
) -> Result<Gpu, WebError> {
    let caps = surface.get_capabilities(&adapter);
    let format = caps
        .formats
        .iter()
        .find(|f| f.is_srgb())
        .or_else(|| caps.formats.first())
        .copied()
        .ok_or_else(|| unavailable("the surface offers no format"))?;
    let config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width: size[0].max(1),
        height: size[1].max(1),
        present_mode: wgpu::PresentMode::Fifo,
        desired_maximum_frame_latency: 2,
        alpha_mode: caps
            .alpha_modes
            .first()
            .copied()
            .unwrap_or(wgpu::CompositeAlphaMode::Auto),
        view_formats: Vec::new(),
        color_space: wgpu::SurfaceColorSpace::default(),
    };
    surface.configure(&device, &config);
    Ok(Gpu {
        backend,
        adapter,
        device,
        queue,
        surface,
        config,
    })
}

/// A device and surface on `canvas`: the requested API, or for `auto` WebGPU with WebGL2 behind it.
pub async fn create(
    canvas: &HtmlCanvasElement,
    choice: BackendChoice,
    size: [u32; 2],
) -> Result<Gpu, WebError> {
    match choice {
        BackendChoice::Webgl2 => on_webgl2(canvas, size).await,
        BackendChoice::Webgpu | BackendChoice::Auto => match on_webgpu(canvas, size).await {
            Ok(gpu) => Ok(gpu),
            Err(webgpu) if choice.falls_back_to_webgl2(webgpu.took_canvas) => {
                on_webgl2(canvas, size).await.map_err(|webgl| {
                    unavailable(format!(
                        "WebGPU: {}; WebGL2: {}",
                        webgpu.error.message, webgl.message
                    ))
                })
            }
            Err(webgpu) if choice == BackendChoice::Auto => Err(unavailable(format!(
                "WebGPU: {} (after it took the canvas, so WebGL2 cannot use this canvas; \
                 create the player on a new canvas with backend \"webgl2\")",
                webgpu.error.message
            ))),
            Err(webgpu) => Err(webgpu.error),
        },
    }
}

impl Gpu {
    /// The surface's texture format.
    pub fn format(&self) -> wgpu::TextureFormat {
        self.config.format
    }
}

impl FrameTarget for Gpu {
    fn resize(&mut self, width: u32, height: u32) {
        let (width, height) = (width.max(1), height.max(1));
        if width != self.config.width || height != self.config.height {
            self.config.width = width;
            self.config.height = height;
            self.surface.configure(&self.device, &self.config);
        }
    }

    fn with_frame(
        &mut self,
        draw: &mut dyn FnMut(&wgpu::Texture) -> Result<(), WebError>,
    ) -> Result<DrawOutcome, WebError> {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                frame
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(DrawOutcome::Skipped);
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return Ok(DrawOutcome::Skipped);
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(WebError::new("render_failed", "the surface refused the frame"));
            }
        };
        draw(&frame.texture)?;
        self.queue.present(frame);
        Ok(DrawOutcome::Drawn)
    }
}
