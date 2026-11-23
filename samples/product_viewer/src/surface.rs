// SPDX-License-Identifier: MIT-0
//! The app's window surface and the panel drawn over the piece. The instance and device live as long as the
//! process; only the surface goes away on a suspend (AGENTS.md § 5). The shared player draws through
//! [`SurfaceTarget`], so a frame is the renderer's picture, then the panel, then the present.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use regia_platform::OsWindow;
use regia_viewer::ViewerStyle;
use regia_web::WebError;
use regia_web::scene::FrameTarget;
use regia_web::seams::DrawOutcome;

/// Why the GPU could not be set up.
#[derive(Debug, thiserror::Error)]
pub enum GpuError {
    /// No surface on this window.
    #[error("cannot create a window surface: {0}")]
    Surface(#[from] wgpu::CreateSurfaceError),
    /// No adapter can present to the surface.
    #[error("no GPU adapter: {0}")]
    Adapter(#[from] wgpu::RequestAdapterError),
    /// The device was refused.
    #[error(transparent)]
    Viewer(#[from] regia_viewer::ViewerError),
}

/// Which graphics API the viewer asks for. Phones with Vulkan never take the GLES path by themselves, so a test
/// run forces it with `--es regia_backend gl` (`--backend gl` on desktop).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendChoice {
    /// What this platform prefers: Vulkan with a GLES fallback on Android.
    #[default]
    Auto,
    /// Vulkan only.
    Vulkan,
    /// OpenGL ES only.
    Gl,
}

impl BackendChoice {
    /// `auto`, `vulkan` or `gl`; `None` for anything else.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "auto" => Some(Self::Auto),
            "vulkan" => Some(Self::Vulkan),
            "gl" => Some(Self::Gl),
            _ => None,
        }
    }

    fn backends(self, os: &str) -> wgpu::Backends {
        match (self, os) {
            (Self::Vulkan, _) => wgpu::Backends::VULKAN,
            (Self::Gl, _) => wgpu::Backends::GL,
            // Vulkan is the path that is measured; a phone without it falls back to GLES.
            (Self::Auto, "android") => wgpu::Backends::VULKAN | wgpu::Backends::GL,
            (Self::Auto, "windows") => wgpu::Backends::VULKAN | wgpu::Backends::DX12,
            (Self::Auto, "macos") => wgpu::Backends::METAL,
            (Self::Auto, _) => wgpu::Backends::VULKAN,
        }
    }
}

fn low_power_adapter(
    instance: &wgpu::Instance,
    surface: Option<&wgpu::Surface<'static>>,
) -> Result<wgpu::Adapter, wgpu::RequestAdapterError> {
    regia_viewer::gpu::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        // The viewer sips power; on a phone there is only one adapter anyway.
        power_preference: wgpu::PowerPreference::LowPower,
        force_fallback_adapter: false,
        compatible_surface: surface,
        apply_limit_buckets: false,
    }))
}

/// A frame read back from the GPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capture {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// RGBA8, top row first.
    pub pixels: Vec<u8>,
}

/// Device-lifetime GPU objects plus the current surface and the panel's picture.
pub struct SurfaceHost {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    format: wgpu::TextureFormat,
    window: Option<Arc<OsWindow>>,
    surface: Option<wgpu::Surface<'static>>,
    needs_configure: bool,
    size: (u32, u32),
    panel: egui_wgpu::Renderer,
    /// The panel's picture.
    primitives: Vec<egui::ClippedPrimitive>,
    /// The panel's buffers on the GPU are older than `primitives`.
    panel_changed: bool,
    pixels_per_point: f32,
    /// Texture changes of the panel, applied at the next draw.
    pending_textures: egui::TexturesDelta,
    surfaces_created: u64,
    surfaces_dropped: u64,
    presents: u64,
    /// A frame could not reach the screen and should be tried again.
    retry: bool,
    capture_next: bool,
    captured: Option<Capture>,
}

impl Drop for SurfaceHost {
    fn drop(&mut self) {
        // The textures die with the device; egui asserts that a dropped delta is empty.
        self.pending_textures.clear();
    }
}

impl SurfaceHost {
    /// Creates the instance and device for `window`, and its first surface.
    pub fn new(
        window: &Arc<OsWindow>,
        display: regia_platform::DisplayHandle,
        size: (u32, u32),
        backend: BackendChoice,
    ) -> Result<Self, GpuError> {
        let _span = tracing::info_span!("gpu_setup").entered();
        let mut descriptor = wgpu::InstanceDescriptor::new_with_display_handle(Box::new(display.0));
        descriptor.backends = backend.backends(std::env::consts::OS);
        descriptor.flags = wgpu::InstanceFlags::empty();
        let instance = wgpu::Instance::new(descriptor);
        let surface = instance.create_surface(window.clone())?;
        let adapter = low_power_adapter(&instance, Some(&surface))?;
        let formats = surface.get_capabilities(&adapter).formats;
        let format = regia_viewer::gpu::preferred_surface_format(&formats)
            .unwrap_or(wgpu::TextureFormat::Rgba8UnormSrgb);
        let mut host = Self::assemble(instance, adapter, format, size)?;
        host.window = Some(window.clone());
        host.surface = Some(surface);
        host.surfaces_created = 1;
        Ok(host)
    }

    /// A device with no window, for tests: frames are captured, never presented. `None` when this machine
    /// offers no adapter.
    #[must_use]
    pub fn headless(size: (u32, u32)) -> Option<Self> {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = BackendChoice::Auto.backends(std::env::consts::OS);
        descriptor.flags = wgpu::InstanceFlags::empty();
        let instance = wgpu::Instance::new(descriptor);
        let adapter = low_power_adapter(&instance, None).ok()?;
        Self::assemble(instance, adapter, wgpu::TextureFormat::Rgba8UnormSrgb, size).ok()
    }

    fn assemble(
        instance: wgpu::Instance,
        adapter: wgpu::Adapter,
        format: wgpu::TextureFormat,
        size: (u32, u32),
    ) -> Result<Self, GpuError> {
        let info = adapter.get_info();
        tracing::info!(
            target: "product_viewer",
            "adapter: {} ({:?}, driver {}), format {format:?}",
            info.name,
            info.backend,
            info.driver_info
        );
        let (device, queue) =
            regia_viewer::gpu::block_on(regia_viewer::gpu::request_viewer_device_with_cache(&adapter))?;
        let panel = egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());
        Ok(Self {
            instance,
            adapter,
            device,
            queue,
            format,
            window: None,
            surface: None,
            needs_configure: true,
            size,
            panel,
            primitives: Vec::new(),
            panel_changed: false,
            pixels_per_point: 1.0,
            pending_textures: egui::TexturesDelta::default(),
            surfaces_created: 0,
            surfaces_dropped: 0,
            presents: 0,
            retry: false,
            capture_next: false,
            captured: None,
        })
    }

    /// The device, for the renderer.
    #[must_use]
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// The queue, for the renderer.
    #[must_use]
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// The adapter.
    #[must_use]
    pub fn adapter(&self) -> &wgpu::Adapter {
        &self.adapter
    }

    /// The format frames are drawn in.
    #[must_use]
    pub fn format(&self) -> wgpu::TextureFormat {
        self.format
    }

    /// The OS took the window away: the surface must not outlive this call.
    pub fn drop_surface(&mut self) {
        if self.surface.take().is_some() {
            self.surfaces_dropped += 1;
        }
        // Ends the use of the surface's images before the OS destroys the native window.
        self.wait_idle();
    }

    /// The window is back: a new surface on the same device.
    pub fn recreate_surface(&mut self, window: &Arc<OsWindow>, size: (u32, u32)) -> Result<(), GpuError> {
        if self.surface.is_none() {
            self.surface = Some(self.instance.create_surface(window.clone())?);
            self.surfaces_created += 1;
        }
        self.window = Some(window.clone());
        self.size = size;
        self.needs_configure = true;
        Ok(())
    }

    /// Surfaces created since start.
    #[must_use]
    pub fn surfaces_created(&self) -> u64 {
        self.surfaces_created
    }

    /// Surfaces dropped since start.
    #[must_use]
    pub fn surfaces_dropped(&self) -> u64 {
        self.surfaces_dropped
    }

    /// Whether a surface exists now.
    #[must_use]
    pub fn has_surface(&self) -> bool {
        self.surface.is_some()
    }

    /// Frames that reached the screen.
    #[must_use]
    pub fn presents(&self) -> u64 {
        self.presents
    }

    /// Takes the "a frame could not reach the screen" flag.
    pub fn take_retry(&mut self) -> bool {
        std::mem::take(&mut self.retry)
    }

    /// Asks the next frame to be read back too (agent stills and tests).
    pub fn capture_next_frame(&mut self) {
        self.capture_next = true;
    }

    /// Forgets a capture asked for and not made (a scripted step that drew nothing).
    pub fn cancel_capture(&mut self) {
        self.capture_next = false;
    }

    /// Takes the frame read back last.
    pub fn take_capture(&mut self) -> Option<Capture> {
        self.captured.take()
    }

    /// Blocks until the GPU finished everything submitted so far.
    pub fn wait_idle(&self) {
        if let Err(err) = self.device.poll(wgpu::PollType::wait_indefinitely()) {
            tracing::warn!(target: "product_viewer", "poll: {err}");
        }
    }

    /// Hands over the panel's new picture.
    pub fn set_panel(
        &mut self,
        primitives: Vec<egui::ClippedPrimitive>,
        textures: egui::TexturesDelta,
        scale: f32,
    ) {
        self.primitives = primitives;
        self.pending_textures.append(textures);
        self.pixels_per_point = scale;
        self.panel_changed = true;
    }

    fn configure(&mut self) -> bool {
        let Some(surface) = &self.surface else {
            return false;
        };
        let (width, height) = self.size;
        if width == 0 || height == 0 {
            return false;
        }
        let caps = surface.get_capabilities(&self.adapter);
        // Fifo: a frame per vsync at most, and the swapchain's wait is the frame pacing.
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: self.format,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps
                .alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto),
            view_formats: vec![],
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&self.device, &config);
        self.needs_configure = false;
        true
    }

    /// Draws the panel over what is in `view`.
    fn draw_panel(&mut self, view: &wgpu::TextureView, size: (u32, u32)) {
        let _span = tracing::info_span!("viewer_panel_draw").entered();
        let mut delta = std::mem::take(&mut self.pending_textures);
        for (id, images) in &delta.set {
            for image in images {
                self.panel.update_texture(&self.device, &self.queue, *id, image);
            }
        }
        for id in &delta.free {
            self.panel.free_texture(id);
        }
        // Dealt with; egui asserts that a dropped delta is empty.
        delta.clear();
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [size.0, size.1],
            pixels_per_point: self.pixels_per_point,
        };
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("viewer panel"),
            });
        // A capture and a present share these buffers; the screen size only changes with a new picture.
        if std::mem::take(&mut self.panel_changed) {
            let extra =
                self.panel
                    .update_buffers(&self.device, &self.queue, &mut encoder, &self.primitives, &screen);
            debug_assert!(extra.is_empty(), "the panel has no paint callbacks");
        }
        if !self.primitives.is_empty() {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("viewer panel"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            self.panel.render(&mut pass, &self.primitives, &screen);
        }
        self.queue.submit([encoder.finish()]);
    }

    /// Renders offscreen with `draw`, adds the panel, reads the frame back.
    fn capture(
        &mut self,
        draw: &mut dyn FnMut(&wgpu::Texture) -> Result<(), WebError>,
    ) -> Result<(), WebError> {
        let (width, height) = (self.size.0.max(1), self.size.1.max(1));
        let extent = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("viewer capture"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        draw(&texture)?;
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.draw_panel(&view, (width, height));
        let failed = |err: String| WebError::new("render_failed", err);
        let row =
            (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewer capture"),
            size: u64::from(row) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("viewer capture"),
            });
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(height),
                },
            },
            extent,
        );
        self.queue.submit([encoder.finish()]);
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|err| failed(err.to_string()))?;
        let mapped = slice.get_mapped_range().map_err(|err| failed(err.to_string()))?;
        let swap = matches!(
            self.format,
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
        );
        let mut pixels = Vec::with_capacity(4 * width as usize * height as usize);
        for line in mapped.chunks_exact(row as usize) {
            let (texels, _) = line[..4 * width as usize].as_chunks::<4>();
            for texel in texels {
                if swap {
                    pixels.extend_from_slice(&[texel[2], texel[1], texel[0], texel[3]]);
                } else {
                    pixels.extend_from_slice(texel);
                }
            }
        }
        drop(mapped);
        buffer.unmap();
        self.captured = Some(Capture {
            width,
            height,
            pixels,
        });
        Ok(())
    }

    /// One frame with no product on it: the backdrop a piece is seen on, the panel over it, the present. The
    /// player only draws with a product open, and the panel reaches the screen on top of its frame, so with
    /// nothing open (a first start, a package refused) the host draws this frame itself.
    pub fn present_panel_only(&mut self) -> Result<DrawOutcome, WebError> {
        let (device, queue) = (self.device.clone(), self.queue.clone());
        let [r, g, b, a] = ViewerStyle::default().clear_color.map(f64::from);
        self.present_frame(&mut |texture| {
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("viewer backdrop"),
            });
            encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("viewer backdrop"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r, g, b, a }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            queue.submit([encoder.finish()]);
            Ok(())
        })
    }

    /// One frame: the renderer's picture on the surface, the panel over it, the present.
    fn present_frame(
        &mut self,
        draw: &mut dyn FnMut(&wgpu::Texture) -> Result<(), WebError>,
    ) -> Result<DrawOutcome, WebError> {
        let _span = tracing::info_span!("viewer_frame_present").entered();
        if std::mem::take(&mut self.capture_next) || self.window.is_none() {
            self.capture(draw)?;
            if self.window.is_none() {
                return Ok(DrawOutcome::Drawn);
            }
            // The panel's buffers were just uploaded for the capture; the frame below reuses them.
        }
        if self.needs_configure && !self.configure() {
            return Ok(DrawOutcome::Skipped);
        }
        let Some(surface) = &self.surface else {
            return Ok(DrawOutcome::Skipped);
        };
        let frame = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => frame,
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                self.needs_configure = true;
                frame
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.needs_configure = true;
                self.retry = true;
                return Ok(DrawOutcome::Skipped);
            }
            wgpu::CurrentSurfaceTexture::Timeout
            | wgpu::CurrentSurfaceTexture::Occluded
            | wgpu::CurrentSurfaceTexture::Validation => {
                self.retry = true;
                return Ok(DrawOutcome::Skipped);
            }
        };
        draw(&frame.texture)?;
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.draw_panel(&view, (frame.texture.width(), frame.texture.height()));
        if let Some(window) = &self.window {
            window.pre_present_notify();
        }
        self.queue.present(frame);
        self.presents += 1;
        Ok(DrawOutcome::Drawn)
    }
}

/// The player's way to the screen. The host keeps the same [`SurfaceHost`] for the lifecycle and the panel.
pub struct SurfaceTarget(pub Rc<RefCell<SurfaceHost>>);

impl FrameTarget for SurfaceTarget {
    fn resize(&mut self, width: u32, height: u32) {
        let mut host = self.0.borrow_mut();
        if host.size != (width, height) {
            host.size = (width, height);
            host.needs_configure = true;
        }
    }

    fn with_frame(
        &mut self,
        draw: &mut dyn FnMut(&wgpu::Texture) -> Result<(), WebError>,
    ) -> Result<DrawOutcome, WebError> {
        self.0.borrow_mut().present_frame(draw)
    }
}

#[cfg(test)]
mod backend_tests {
    use super::BackendChoice;

    #[test]
    fn a_forced_backend_asks_for_that_api_alone() {
        for os in ["android", "linux", "windows", "macos"] {
            assert_eq!(BackendChoice::Gl.backends(os), wgpu::Backends::GL, "{os}");
            assert_eq!(BackendChoice::Vulkan.backends(os), wgpu::Backends::VULKAN, "{os}");
        }
    }

    #[test]
    fn a_phone_with_no_choice_prefers_vulkan_and_can_fall_back_to_gles() {
        assert_eq!(
            BackendChoice::default().backends("android"),
            wgpu::Backends::VULKAN | wgpu::Backends::GL
        );
        assert_eq!(BackendChoice::default().backends("linux"), wgpu::Backends::VULKAN);
    }

    #[test]
    fn only_the_three_names_parse() {
        assert_eq!(BackendChoice::parse("gl"), Some(BackendChoice::Gl));
        assert_eq!(BackendChoice::parse("vulkan"), Some(BackendChoice::Vulkan));
        assert_eq!(BackendChoice::parse("auto"), Some(BackendChoice::Auto));
        assert_eq!(BackendChoice::parse("GL"), None);
        assert_eq!(BackendChoice::parse(""), None);
    }
}
