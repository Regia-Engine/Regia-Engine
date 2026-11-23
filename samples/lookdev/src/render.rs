// SPDX-License-Identifier: MIT-0
//! Offscreen terrain frames with a timestamp between each pass.
//!
//! The sample owns the targets. [`regia_terrain::gpu::render_offscreen`] draws shadow, sky, and
//! terrain+scatter+water as one colour pass and does not time them. Look-dev splits scatter
//! and water into their own load passes so `shots.json` can report a millisecond column per
//! pass. Timestamps are written only when the device has both `TIMESTAMP_QUERY` and
//! `TIMESTAMP_QUERY_INSIDE_ENCODERS`. A failed timestamp read disables timing and keeps the
//! pixels; a failed pixel read fails the shot.

use std::time::Duration;

use regia_render::RenderDevice;
use regia_terrain::gpu::{TerrainCamera, TerrainFrame, TerrainGpu, TerrainRenderer, TimeOfDay};
use regia_terrain::{LodSelection, ScatterInstance, select_nodes};

use crate::measure::PassMillis;
use crate::portal::PortalDraw;
use crate::scene::{MacroMap, Scene, fog_for};
use crate::shots::{self, ProfileKind, ShotDesc};
use crate::{LookdevConfig, LookdevError};

/// One displayed frame: tightly packed RGBA8 (top row first) and the pass times.
pub struct FrameCapture {
    /// sRGB framebuffer bytes, `width * height * 4`.
    pub pixels: Vec<u8>,
    /// Per-pass GPU time. Zero when the device has no timestamps or a read failed.
    pub passes: PassMillis,
}

/// One planet's pipelines, reused across every camera and profile.
pub struct TerrainSession {
    gpu: TerrainGpu,
    portal: PortalDraw,
    /// Kept alive so the uploaded layer height maps stay resident. The terrain
    /// shader samples them only after the macro lane lands.
    layer_heights: Vec<wgpu::Texture>,
    macro_map: wgpu::Texture,
    layer_height_bytes: u64,
    macro_bytes: u64,
    timed: bool,
    timer: Option<Timer>,
    targets: Option<Targets>,
    scatter_profile: Option<ProfileKind>,
    warned_empty: bool,
}

struct Timer {
    set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    staging: wgpu::Buffer,
}

struct Targets {
    width: u32,
    height: u32,
    colour: wgpu::Texture,
    colour_view: wgpu::TextureView,
    depth_view: wgpu::TextureView,
    readback: wgpu::Buffer,
    row_bytes: u32,
    padded_row: u32,
}

const TIMESTAMP_QUERIES: u32 = 6;
const PORTAL_RING: usize = 8;

impl TerrainSession {
    /// Uploads `scene` and compiles the terrain pipelines once.
    ///
    /// # Errors
    ///
    /// [`LookdevError::Terrain`] when the heightfield or the layers are rejected.
    /// [`LookdevError::Config`] when a height map or the portal mesh is the wrong shape.
    pub fn new(device: &RenderDevice, scene: &Scene) -> Result<Self, LookdevError> {
        let gpu = TerrainGpu::new(
            device,
            &scene.map,
            &scene.layers,
            &scene.cdlod,
            scene.root_level,
            Some(&scene.holes),
            None,
        )?;
        let (layer_heights, layer_height_bytes) = upload_layer_heights(device, scene)?;
        let (macro_map, macro_bytes) = upload_macro(device, &scene.macro_variation)?;
        let portal = PortalDraw::new(&device.device, &device.queue, &scene.tunnel)?;
        tracing::info!(
            target: "lookdev",
            layer_height_bytes,
            macro_bytes,
            macro_resolution = scene.macro_variation.resolution,
            "uploaded layer height maps and macro variation"
        );
        let features = device.device.features();
        let timed = features.contains(wgpu::Features::TIMESTAMP_QUERY)
            && features.contains(wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS);
        let timer = if timed {
            Some(Timer::new(&device.device))
        } else {
            None
        };
        Ok(Self {
            gpu,
            portal,
            layer_heights,
            macro_map,
            layer_height_bytes,
            macro_bytes,
            timed,
            timer,
            targets: None,
            scatter_profile: None,
            warned_empty: false,
        })
    }

    /// Payload bytes of the layer height maps written to the GPU.
    #[must_use]
    pub fn layer_height_bytes(&self) -> u64 {
        let _resident = !self.layer_heights.is_empty();
        self.layer_height_bytes
    }

    /// Payload bytes of the macro-variation map written to the GPU.
    #[must_use]
    pub fn macro_bytes(&self) -> u64 {
        let _resident = self.macro_map.size().width;
        self.macro_bytes
    }

    /// `true` while per-pass timestamps are being recorded.
    #[must_use]
    pub fn gpu_timing(&self) -> bool {
        self.timed
    }

    /// Draws one frame of `shot`.
    ///
    /// # Errors
    ///
    /// [`LookdevError::Render`] when the device poll or the pixel readback fails.
    pub fn render_frame(
        &mut self,
        device: &RenderDevice,
        scene: &Scene,
        shot: ShotDesc,
        frame_index: u32,
        config: &LookdevConfig,
    ) -> Result<FrameCapture, LookdevError> {
        let _span = tracing::info_span!(
            "lookdev_frame",
            planet = scene.planet.name(),
            camera = shot.camera.name(),
            profile = shot.profile.name(),
            frame = frame_index,
        )
        .entered();

        let (width, height) = shots::framebuffer_size(config, shot.profile);
        self.ensure_targets(device, width, height);
        self.ensure_scatter(scene, shot.profile, config.scatter_cap);

        let look = shots::look_camera(scene, shot.camera);
        let aspect = width as f32 / height.max(1) as f32;
        let camera = TerrainCamera {
            position: look.position,
            target: look.target,
            fov_y_radians: shots::fov_y(shot.profile, look.fov_y, aspect),
            aspect,
            near: 0.08,
            far: 12_000.0,
        };
        let selection = self.select_lod(scene, shot.profile, &camera);
        let sun = shots::shot_sun(&scene.climate, shot.camera);
        let frame = terrain_frame(scene, shot, &camera, frame_index);
        self.gpu.update_frame(&frame);
        self.gpu.upload_nodes(&selection);
        self.portal.write_frame(&device.queue, &camera, &sun);
        self.submit_and_read(device, &selection)
    }

    fn ensure_scatter(&mut self, scene: &Scene, profile: ProfileKind, cap: usize) {
        if self.scatter_profile == Some(profile) {
            return;
        }
        let instances = cap_scatter(&scene.scatter, cap, profile.scatter_scale());
        let (fade_start, fade_end) = profile.fade();
        // Rocks take some of the surface normal so they sit on slopes. Full alignment
        // plants them on cliffs; zero leaves them floating above a grade.
        self.gpu.set_scatter(&instances, fade_start, fade_end, 0.55);
        self.scatter_profile = Some(profile);
    }

    fn select_lod(&mut self, scene: &Scene, profile: ProfileKind, camera: &TerrainCamera) -> LodSelection {
        let mut config = scene.cdlod.clone();
        config.range_factor = profile.range_factor();
        let eye_xz = [camera.position[0], camera.position[2]];
        let frustum = view_frustum(camera);
        let mut selection = select_nodes(
            &scene.map,
            &scene.pyramid,
            &config,
            scene.root_level,
            eye_xz,
            Some(&frustum),
        );
        if selection.nodes.is_empty() {
            selection = select_nodes(
                &scene.map,
                &scene.pyramid,
                &config,
                scene.root_level,
                eye_xz,
                None,
            );
        }
        if selection.nodes.is_empty() && !self.warned_empty {
            self.warned_empty = true;
            tracing::warn!(target: "lookdev", "CDLOD selected no nodes");
        }
        selection
    }

    fn ensure_targets(&mut self, device: &RenderDevice, width: u32, height: u32) {
        if self
            .targets
            .as_ref()
            .is_some_and(|targets| targets.width == width && targets.height == height)
        {
            return;
        }
        let colour_format = self.gpu.colour_format();
        let depth_format = self.gpu.depth_format();
        let d = &device.device;
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let colour = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("lookdev color"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: colour_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let colour_view = colour.create_view(&wgpu::TextureViewDescriptor::default());
        let depth = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("lookdev depth"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: depth_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        let row_bytes = width.saturating_mul(4);
        let padded_row =
            row_bytes.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let readback = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lookdev readback"),
            size: u64::from(padded_row) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        self.targets = Some(Targets {
            width,
            height,
            colour,
            colour_view,
            depth_view,
            readback,
            row_bytes,
            padded_row,
        });
    }

    fn submit_and_read(
        &mut self,
        device: &RenderDevice,
        selection: &LodSelection,
    ) -> Result<FrameCapture, LookdevError> {
        let timed = self.timed;
        let query = if timed {
            self.timer.as_ref().map(|timer| timer.set.clone())
        } else {
            None
        };
        let targets = self
            .targets
            .as_ref()
            .expect("color target allocated for this frame");
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("lookdev frame"),
            });

        if let Some(set) = &query {
            encoder.write_timestamp(set, 0);
        }
        self.gpu.encode_sun_shadow(&mut encoder);
        if let Some(set) = &query {
            encoder.write_timestamp(set, 1);
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("lookdev sky"),
                color_attachments: &[Some(colour_attachment(
                    &targets.colour_view,
                    Some(wgpu::Color {
                        r: 0.02,
                        g: 0.03,
                        b: 0.05,
                        a: 1.0,
                    }),
                ))],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.gpu.draw_sky(&mut pass);
        }
        if let Some(set) = &query {
            encoder.write_timestamp(set, 2);
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("lookdev terrain"),
                color_attachments: &[Some(colour_attachment(&targets.colour_view, None))],
                depth_stencil_attachment: Some(depth_attachment(&targets.depth_view, true)),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.gpu.draw_terrain(&mut pass, selection);
            self.portal.draw(&mut pass);
        }
        if let Some(set) = &query {
            encoder.write_timestamp(set, 3);
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("lookdev scatter"),
                color_attachments: &[Some(colour_attachment(&targets.colour_view, None))],
                depth_stencil_attachment: Some(depth_attachment(&targets.depth_view, false)),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.gpu.draw_scatter(&mut pass);
        }
        if let Some(set) = &query {
            encoder.write_timestamp(set, 4);
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("lookdev water"),
                color_attachments: &[Some(colour_attachment(&targets.colour_view, None))],
                depth_stencil_attachment: Some(depth_attachment(&targets.depth_view, false)),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.gpu.draw_water(&mut pass);
        }
        if let Some(set) = &query {
            encoder.write_timestamp(set, 5);
            if let Some(timer) = &self.timer {
                encoder.resolve_query_set(&timer.set, 0..TIMESTAMP_QUERIES, &timer.resolve, 0);
                encoder.copy_buffer_to_buffer(
                    &timer.resolve,
                    0,
                    &timer.staging,
                    0,
                    u64::from(TIMESTAMP_QUERIES) * 8,
                );
            }
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &targets.colour,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &targets.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(targets.padded_row),
                    rows_per_image: Some(targets.height),
                },
            },
            wgpu::Extent3d {
                width: targets.width,
                height: targets.height,
                depth_or_array_layers: 1,
            },
        );

        device.queue.submit([encoder.finish()]);

        let (pixel_tx, pixel_rx) = std::sync::mpsc::channel();
        let mut timer_rx = None;
        {
            let targets = self
                .targets
                .as_ref()
                .expect("color target allocated for this frame");
            targets
                .readback
                .slice(..)
                .map_async(wgpu::MapMode::Read, move |result| {
                    let _ = pixel_tx.send(result);
                });
            if query.is_some()
                && let Some(timer) = &self.timer
            {
                let (tx, rx) = std::sync::mpsc::channel();
                timer
                    .staging
                    .slice(..)
                    .map_async(wgpu::MapMode::Read, move |result| {
                        let _ = tx.send(result);
                    });
                timer_rx = Some(rx);
            }
        }
        device
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(regia_render::RenderError::Poll)?;

        let passes = match timer_rx {
            Some(rx) => match rx.recv_timeout(Duration::from_secs(30)) {
                Ok(Ok(())) => self.read_timestamps(device),
                Ok(Err(err)) => {
                    self.disable_timing(&err);
                    PassMillis::default()
                }
                Err(_) => {
                    self.disable_timing("timestamp readback timed out");
                    PassMillis::default()
                }
            },
            None => PassMillis::default(),
        };

        let pixels = self.read_pixels(&pixel_rx)?;
        Ok(FrameCapture { pixels, passes })
    }

    fn read_timestamps(&mut self, device: &RenderDevice) -> PassMillis {
        let read = self.copy_timestamps();
        match read {
            Ok(stamps) => {
                let period = device.queue.get_timestamp_period();
                let ms = |from: usize, to: usize| {
                    stamps[to].saturating_sub(stamps[from]) as f32 * period / 1_000_000.0
                };
                PassMillis::from_passes(ms(0, 1), ms(1, 2), ms(2, 3), ms(3, 4), ms(4, 5))
            }
            Err(message) => {
                self.disable_timing(&message);
                PassMillis::default()
            }
        }
    }

    fn copy_timestamps(&self) -> Result<[u64; TIMESTAMP_QUERIES as usize], String> {
        let Some(timer) = &self.timer else {
            return Err("timestamp query missing".into());
        };
        let outcome = match timer.staging.slice(..).get_mapped_range() {
            Ok(mapped) => {
                if mapped.len() < TIMESTAMP_QUERIES as usize * 8 {
                    Err("timestamp buffer short".into())
                } else {
                    let mut stamps = [0_u64; TIMESTAMP_QUERIES as usize];
                    for (index, slot) in stamps.iter_mut().enumerate() {
                        let start = index * 8;
                        let mut bytes = [0_u8; 8];
                        bytes.copy_from_slice(&mapped[start..start + 8]);
                        *slot = u64::from_le_bytes(bytes);
                    }
                    Ok(stamps)
                }
            }
            Err(err) => Err(err.to_string()),
        };
        timer.staging.unmap();
        outcome
    }

    fn read_pixels(
        &self,
        pixel_rx: &std::sync::mpsc::Receiver<Result<(), wgpu::BufferAsyncError>>,
    ) -> Result<Vec<u8>, LookdevError> {
        let targets = self
            .targets
            .as_ref()
            .expect("color target allocated for this frame");
        match pixel_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                return Err(LookdevError::Render(regia_render::RenderError::Readback(
                    err.to_string(),
                )));
            }
            Err(_) => {
                return Err(LookdevError::Render(regia_render::RenderError::Readback(
                    "pixel readback timed out".into(),
                )));
            }
        }
        let mapped = targets
            .readback
            .slice(..)
            .get_mapped_range()
            .map_err(|err| LookdevError::Render(regia_render::RenderError::Readback(err.to_string())))?;
        let mut pixels =
            Vec::with_capacity(usize::try_from(targets.row_bytes).unwrap_or(0) * targets.height as usize);
        let row = targets.row_bytes as usize;
        let padded = targets.padded_row as usize;
        for y in 0..targets.height as usize {
            let start = y * padded;
            let end = start + row;
            if end > mapped.len() {
                drop(mapped);
                targets.readback.unmap();
                return Err(LookdevError::Render(regia_render::RenderError::Readback(
                    "pixel readback shorter than the framebuffer".into(),
                )));
            }
            pixels.extend_from_slice(&mapped[start..end]);
        }
        drop(mapped);
        targets.readback.unmap();
        Ok(pixels)
    }

    fn disable_timing(&mut self, err: impl std::fmt::Display) {
        if self.timed {
            tracing::warn!(target: "lookdev", %err, "timestamp read failed; per-pass GPU times disabled");
        }
        self.timed = false;
    }
}

impl Timer {
    fn new(device: &wgpu::Device) -> Self {
        let set = device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("lookdev timestamps"),
            ty: wgpu::QueryType::Timestamp,
            count: TIMESTAMP_QUERIES,
        });
        let bytes = u64::from(TIMESTAMP_QUERIES) * 8;
        let resolve = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lookdev timestamp resolve"),
            size: bytes,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let staging = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lookdev timestamp staging"),
            size: bytes,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Self {
            set,
            resolve,
            staging,
        }
    }
}

fn upload_layer_heights(
    device: &RenderDevice,
    scene: &Scene,
) -> Result<(Vec<wgpu::Texture>, u64), LookdevError> {
    let mut textures = Vec::with_capacity(scene.layer_height_maps.len());
    let mut bytes = 0_u64;
    for (index, map) in scene.layer_height_maps.iter().enumerate() {
        let texture = upload_r32(
            &device.device,
            &device.queue,
            "lookdev layer height",
            map.resolution,
            &map.samples,
            &map.name,
        )?;
        bytes += image_payload_bytes(map.resolution);
        let _ = index;
        textures.push(texture);
    }
    Ok((textures, bytes))
}

fn upload_macro(device: &RenderDevice, map: &MacroMap) -> Result<(wgpu::Texture, u64), LookdevError> {
    let texture = upload_r32(
        &device.device,
        &device.queue,
        "lookdev macro variation",
        map.resolution,
        &map.samples,
        "macro",
    )?;
    Ok((texture, image_payload_bytes(map.resolution)))
}

/// `resolution² × 4` bytes of `R32Float` samples, before row padding.
pub(crate) fn image_payload_bytes(resolution: u32) -> u64 {
    u64::from(resolution) * u64::from(resolution) * 4
}

fn upload_r32(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &'static str,
    resolution: u32,
    samples: &[f32],
    name: &str,
) -> Result<wgpu::Texture, LookdevError> {
    let expected = (resolution as usize).saturating_mul(resolution as usize);
    if resolution < 2 || samples.len() != expected {
        return Err(LookdevError::Config(format!(
            "{name} height map has {} samples, expected {expected}",
            samples.len()
        )));
    }
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: resolution,
            height: resolution,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R32Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let row_bytes = resolution.saturating_mul(4);
    let padded = row_bytes.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let mut packed = vec![0_u8; padded as usize * resolution as usize];
    for row in 0..resolution as usize {
        let dst = row * padded as usize;
        for col in 0..resolution as usize {
            let sample = samples[row * resolution as usize + col];
            let start = dst + col * 4;
            packed[start..start + 4].copy_from_slice(&sample.to_le_bytes());
        }
    }
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &packed,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(padded),
            rows_per_image: Some(resolution),
        },
        wgpu::Extent3d {
            width: resolution,
            height: resolution,
            depth_or_array_layers: 1,
        },
    );
    Ok(texture)
}

fn terrain_frame(scene: &Scene, shot: ShotDesc, camera: &TerrainCamera, frame_index: u32) -> TerrainFrame {
    let sun = shots::shot_sun(&scene.climate, shot.camera);
    let tod = TimeOfDay {
        azimuth_radians: sun.azimuth,
        elevation_radians: sun.elevation,
        sun_color: sun.color,
        illuminance_lux: sun.lux,
        ambient: sun.ambient,
    };
    let wetness = (scene.climate.wetness * shot.profile.wetness_scale()).clamp(0.0, 1.0);
    // `from_camera` fills every field the hook knows about, including ones added by a
    // later merge. Fog and wetness are the look-dev overrides on top of that.
    let mut frame = TerrainFrame::from_camera(camera, &tod, wetness, frame_index as f32 / 30.0);
    frame.fog = fog_for(&scene.climate, shot.profile.fog_scale());
    frame.wetness = wetness;
    frame
}

fn view_frustum(camera: &TerrainCamera) -> regia_core::math::Frustum {
    let matrix = regia_core::math::Mat4::from_cols_array_2d(&camera.view_projection());
    regia_core::math::Frustum::from_view_projection(&matrix)
}

fn colour_attachment(
    view: &wgpu::TextureView,
    clear: Option<wgpu::Color>,
) -> wgpu::RenderPassColorAttachment<'_> {
    wgpu::RenderPassColorAttachment {
        view,
        depth_slice: None,
        resolve_target: None,
        ops: wgpu::Operations {
            load: match clear {
                Some(color) => wgpu::LoadOp::Clear(color),
                None => wgpu::LoadOp::Load,
            },
            store: wgpu::StoreOp::Store,
        },
    }
}

fn depth_attachment(view: &wgpu::TextureView, clear: bool) -> wgpu::RenderPassDepthStencilAttachment<'_> {
    wgpu::RenderPassDepthStencilAttachment {
        view,
        depth_ops: Some(wgpu::Operations {
            load: if clear {
                wgpu::LoadOp::Clear(1.0)
            } else {
                wgpu::LoadOp::Load
            },
            store: wgpu::StoreOp::Store,
        }),
        stencil_ops: None,
    }
}

/// Keeps `scale * cap` of the body (everything before the portal ring) and always
/// appends the ring. The ring is the last [`PORTAL_RING`] instances.
pub(crate) fn cap_scatter(instances: &[ScatterInstance], cap: usize, scale: f32) -> Vec<ScatterInstance> {
    let ring = PORTAL_RING.min(instances.len());
    let body_end = instances.len() - ring;
    let keep_f = (cap as f32) * scale;
    let keep = if keep_f.is_finite() && keep_f > 0.0 {
        let rounded = keep_f.round();
        if rounded >= body_end as f32 {
            body_end
        } else {
            rounded as usize
        }
    } else {
        0
    };
    let mut out = instances[..keep].to_vec();
    out.extend_from_slice(&instances[body_end..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rock(index: f32) -> ScatterInstance {
        ScatterInstance {
            position: [index, 0.0, 0.0],
            yaw: 0.0,
            scale: 1.0,
            normal: [0.0, 1.0, 0.0],
            tint: [0.4, 0.4, 0.4],
        }
    }

    #[test]
    fn cap_keeps_a_fraction_of_the_body_and_the_whole_portal_ring() {
        let instances: Vec<_> = (0..20).map(|i| rock(i as f32)).collect();
        let kept = cap_scatter(&instances, 10, 0.5);
        assert_eq!(kept.len(), 5 + PORTAL_RING);
        assert_eq!(kept[0].position[0], 0.0);
        assert_eq!(kept[4].position[0], 4.0);
        assert_eq!(kept[5].position[0], 12.0);
        assert_eq!(kept.last().expect("ring").position[0], 19.0);
    }

    #[test]
    fn cap_with_a_short_body_keeps_every_rock() {
        let instances: Vec<_> = (0..10).map(|i| rock(i as f32)).collect();
        let kept = cap_scatter(&instances, 4, 1.0);
        assert_eq!(kept.len(), 10);
    }

    #[test]
    fn range_factor_on_a_cloned_config_does_not_change_the_grid() {
        let mut config = regia_terrain::CdlodConfig::default();
        let grid = config.grid_size;
        config.range_factor = ProfileKind::DeckMedium.range_factor();
        assert_eq!(config.grid_size, grid);
        assert!(config.range_factor >= 2.0);
    }
}
