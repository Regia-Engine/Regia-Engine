// SPDX-License-Identifier: MPL-2.0
//! The raster cloud passes as measurements (ADR-2127): the real `fs_cloud_march` and
//! `fs_cloud_shadow_map` of `clouds.wgsl`, drawn for a given camera and light into float targets
//! and read back, so the reference path tracer's cloud field has something to be compared with.
//! Not used by any frame; `regia_pt::cloud_compare` and tests call it.

use crate::frame_work::counted::*;
use regia_core::glam::Vec3;

use super::{
    CLOUD_DIST_FORMAT, CLOUD_MAP_FORMAT, OutdoorComposite, OutdoorState, cloud_ambient, cloud_map_uniform,
    cloud_uniform, depth_tex, draw_march, draw_pass, look, rgba_tex, submit,
};
use crate::atmosphere::AtmosphereLuts;
use crate::clouds::{CLOUD_FADE_M, CloudSettings};
use crate::{CloudMarch, RenderDevice, RenderError};

/// The camera and the light of a [`render_cloud_view`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CloudViewDesc {
    /// Image width, texels (one march per texel, the frame's divisor 1).
    pub width: u32,
    /// Image height, texels.
    pub height: u32,
    /// Eye, world metres.
    pub eye: [f32; 3],
    /// Point the camera looks at, world metres (up is +Y).
    pub target: [f32; 3],
    /// Vertical field of view, radians.
    pub fov_y: f32,
    /// Unit vector toward the sun.
    pub to_sun: [f32; 3],
    /// Sun colour × illuminance, the same at the layer's base and top.
    pub sun: [f32; 3],
    /// Ambient radiance at the cloud tops and bases ([`cloud_ambient_radiance`]).
    pub ambient: ([f32; 3], [f32; 3]),
    /// Radius of the planet the shells curve with, metres.
    pub planet_radius_m: f32,
    /// The cone light march, curl swirl and dark edge (Desktop High and up).
    pub cone_light: bool,
    /// Steps and marches per texel (the frame's `FrameQuality::cloud_march`).
    pub march: CloudMarch,
    /// Percent of the layer's steps (the frame's `FrameQuality::cloud_step_percent`).
    pub step_percent: u32,
    /// Strength of the two multiple-scattering octaves (0.5 in a frame). 0 leaves the march's
    /// single-scattering term when the look has no diffusion.
    pub multiple_scattering: f32,
    /// The distance fade (the frame's [`CLOUD_FADE_M`]). Measurements of the cloud alone pass
    /// [`Self::NO_FADE_M`], which also opens the span's clamp at 1.5 fades.
    pub fade_m: f32,
    /// Frames of the march's start jitter to average (`frame % 64`), at least 1.
    pub frames: u32,
}

impl CloudViewDesc {
    /// A fade far beyond any cloud: the march's result is the cloud alone.
    pub const NO_FADE_M: f32 = 1.0e9;

    /// The frame's march (64 steps on Desktop High, one march per texel, the frame's fade) at a
    /// camera and a light, with [`Self::ambient`] left black for the caller.
    #[must_use]
    pub fn realtime(width: u32, height: u32, eye: [f32; 3], target: [f32; 3], fov_y: f32) -> Self {
        Self {
            width,
            height,
            eye,
            target,
            fov_y,
            to_sun: [0.0, 1.0, 0.0],
            sun: [0.0; 3],
            ambient: ([0.0; 3], [0.0; 3]),
            planet_radius_m: 6_360_000.0,
            cone_light: true,
            march: CloudMarch::REALTIME,
            step_percent: 100,
            multiple_scattering: 0.5,
            fade_m: CLOUD_FADE_M,
            frames: 1,
        }
    }
}

/// What [`render_cloud_view`] measured.
#[derive(Clone, Debug, PartialEq)]
pub struct CloudViewImage {
    /// Width, texels.
    pub width: u32,
    /// Height, texels.
    pub height: u32,
    /// Row-major, top row first: the in-scattered radiance (rgb, cd/m²) and the view
    /// transmittance (a): a frame composites `sky · a + rgb`.
    pub rgba: Vec<[f32; 4]>,
}

/// The ambient radiance the frame's cloud march reads: the sky dome at the cloud tops, sky plus
/// ground bounce at the bases.
#[must_use]
pub fn cloud_ambient_radiance(luts: &AtmosphereLuts) -> ([f32; 3], [f32; 3]) {
    cloud_ambient(luts)
}

fn float_target(device: &RenderDevice, width: u32, height: u32, format: wgpu::TextureFormat) -> super::Tex {
    rgba_tex(
        &device.device,
        width,
        height,
        format,
        wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        "cloud probe",
    )
}

/// Half floats out of a target, row-major, `channels` per texel.
pub(super) fn read_half(
    device: &RenderDevice,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
    channels: u32,
) -> Result<Vec<f32>, RenderError> {
    let bpp = channels * 2;
    let row = (width * bpp).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let d = &device.device;
    let buffer = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("cloud probe readback"),
        size: u64::from(row) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("cloud probe readback"),
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
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    submit(device, encoder)?;
    let slice = buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    d.poll(wgpu::PollType::wait_indefinitely())
        .map_err(RenderError::Poll)?;
    rx.recv()
        .map_err(|e| RenderError::Readback(e.to_string()))?
        .map_err(|e| RenderError::Readback(e.to_string()))?;
    let mapped = slice
        .get_mapped_range()
        .map_err(|e| RenderError::Readback(e.to_string()))?;
    let mut out = Vec::with_capacity((width * height * channels) as usize);
    for y in 0..height as usize {
        let start = y * row as usize;
        let bytes = &mapped[start..start + (width * bpp) as usize];
        out.extend(
            bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| half_to_f32(u16::from_le_bytes(*b))),
        );
    }
    drop(mapped);
    buffer.unmap();
    Ok(out)
}

/// IEEE 754 binary16 to f32.
fn half_to_f32(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exponent = i32::from((h >> 10) & 0x1f);
    let fraction = f32::from(h & 0x3ff);
    sign * match exponent {
        0 => fraction * (-24.0_f32).exp2(),
        31 if fraction == 0.0 => f32::INFINITY,
        31 => f32::NAN,
        e => (1.0 + fraction / 1024.0) * ((e - 15) as f32).exp2(),
    }
}

/// Draws the raster cloud layer of `settings` for `desc` with the frame's own march and reads the
/// result back: no history, no upsample, no air in front (the frame's aerial table is not bound),
/// so the numbers are the cloud alone.
///
/// # Errors
/// [`RenderError`] when a pass or the readback fails.
#[tracing::instrument(skip_all)]
pub fn render_cloud_view(
    device: &RenderDevice,
    settings: &CloudSettings,
    desc: &CloudViewDesc,
) -> Result<CloudViewImage, RenderError> {
    let (width, height) = (desc.width.max(1), desc.height.max(1));
    let mut state = OutdoorState::default();
    state.params.clouds = *settings;
    state.ensure_gpu(&device.device, device.shared_pipelines())?;
    let view = look(
        Vec3::from_array(desc.eye),
        Vec3::from_array(desc.target),
        Vec3::Y,
        desc.fov_y,
        width as f32 / height as f32,
        0.3,
        400_000.0,
    );
    let depth = depth_tex(&device.device, width, height, "cloud probe depth");
    let color = float_target(device, width, height, wgpu::TextureFormat::Rgba16Float);
    let dist = float_target(device, width, height, CLOUD_DIST_FORMAT);
    let lux = 1.0_f32;
    let shot = OutdoorComposite {
        prepared_sky: None,
        depth_is_scene: false,
        pre_exposure: 1.0,
        hdr: &color.view,
        depth: &depth.view,
        shadow: None,
        width,
        height,
        format: wgpu::TextureFormat::Rgba16Float,
        inv_view_proj: view.inv_view_proj,
        unjittered_inv_view_proj: view.inv_view_proj,
        prev_view_proj: view.view_proj,
        eye: view.eye,
        near: 0.3,
        far: 400_000.0,
        sun_travel: [-desc.to_sun[0], -desc.to_sun[1], -desc.to_sun[2]],
        sun_color: desc.sun,
        sun_illuminance: lux,
        light_vp: view.view_proj,
        shadow_size: 0.0,
        atmosphere: false,
        fog: false,
        clouds: true,
        unjittered_view_proj: view.view_proj,
        cloud_reprojection: false,
        cloud_step_percent: desc.step_percent,
        cloud_cone_light: desc.cone_light,
        cloud_march: desc.march,
        local_shadow: None,
        fog_light_shadows: [[[0.0; 4]; 6]; 4],
    };
    let tracked = crate::device::TrackedQueue::observe(&device.queue);
    let gpu = state
        .gpu
        .as_mut()
        .ok_or_else(|| RenderError::Readback("cloud probe: no outdoor GPU state".into()))?;
    let weather = gpu.ensure_weather(&device.device, &tracked, settings, None);
    gpu.ensure_cloud_noise(&device.device, &tracked);
    let mut bytes = cloud_uniform(&shot, settings, 0, desc.planet_radius_m, Some(desc.ambient));
    bytes.hdr[1] = 1.0;
    bytes.sun_color[3] = desc.multiple_scattering.max(0.0);
    bytes.ambient_top[3] = desc.fade_m.max(1.0);
    bytes.sun_bottom[3] = 0.0;
    let dummy = gpu.cloud_placeholder();
    let bg = gpu.cloud_bind(
        &device.device,
        &weather,
        &depth.view,
        [&dummy, &dummy, &dummy, &dummy],
    );
    let pipe = gpu.cloud_pipe(&device.device, wgpu::TextureFormat::Rgba16Float, 2);
    let frames = desc.frames.max(1);
    let count = (width * height) as usize;
    let mut sum = vec![[0.0_f64; 4]; count];
    // A depth target cleared to the far plane: every texel sees the sky.
    let mut clear = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("cloud probe depth"),
        });
    drop(clear.counted_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("cloud probe depth"),
        color_attachments: &[],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &depth.view,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Clear(1.0),
                store: wgpu::StoreOp::Store,
            }),
            stencil_ops: None,
        }),
        timestamp_writes: None,
        ..Default::default()
    }));
    submit(device, clear)?;
    for frame in 0..frames {
        bytes.camera[3] = (frame % 64) as f32;
        device
            .queue
            .write_buffer(&gpu.cloud_buf, 0, bytemuck::bytes_of(&bytes));
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("cloud probe"),
            });
        draw_march(
            &mut encoder,
            &pipe,
            &bg,
            &color.view,
            &dist.view,
            "cloud probe march",
        );
        submit(device, encoder)?;
        let halves = read_half(device, &color.texture, width, height, 4)?;
        for (s, h) in sum.iter_mut().zip(halves.as_chunks::<4>().0) {
            for (a, b) in s.iter_mut().zip(h) {
                *a += f64::from(*b);
            }
        }
    }
    let n = f64::from(frames);
    Ok(CloudViewImage {
        width,
        height,
        rgba: sum.iter().map(|s| s.map(|v| (v / n) as f32)).collect(),
    })
}

/// A square window of the raster's cloud transmittance map (ADR-320) around the origin: the sun's
/// Beer–Lambert transmittance through the layer at each base-plane cell, from the real
/// `fs_cloud_shadow_map` (16 midpoint samples of `density_at`). `size` texels of `texel_m` metres
/// on a side, row-major. A texel `(x, y)` holds the cell `(x − size/2, y − size/2)`.
///
/// # Errors
/// [`RenderError`] when a pass or the readback fails.
#[tracing::instrument(skip_all)]
pub fn render_cloud_sun_map(
    device: &RenderDevice,
    settings: &CloudSettings,
    to_sun: [f32; 3],
    planet_radius_m: f32,
    cone_light: bool,
    size: u32,
    texel_m: f32,
) -> Result<Vec<f32>, RenderError> {
    let size = size.max(1);
    let mut state = OutdoorState::default();
    state.params.clouds = *settings;
    state.ensure_gpu(&device.device, device.shared_pipelines())?;
    let depth = depth_tex(&device.device, 1, 1, "cloud probe depth");
    let target = float_target(device, size, size, CLOUD_MAP_FORMAT);
    let tracked = crate::device::TrackedQueue::observe(&device.queue);
    let gpu = state
        .gpu
        .as_mut()
        .ok_or_else(|| RenderError::Readback("cloud probe: no outdoor GPU state".into()))?;
    let weather = gpu.ensure_weather(&device.device, &tracked, settings, None);
    gpu.ensure_cloud_noise(&device.device, &tracked);
    let half = (size / 2) as i32;
    let bytes = cloud_map_uniform(
        settings,
        planet_radius_m,
        to_sun,
        cone_light,
        [-half, -half],
        size,
        texel_m,
    );
    device
        .queue
        .write_buffer(&gpu.cloud_buf, 0, bytemuck::bytes_of(&bytes));
    let dummy = gpu.cloud_placeholder();
    let bg = gpu.cloud_bind(
        &device.device,
        &weather,
        &depth.view,
        [&dummy, &dummy, &dummy, &dummy],
    );
    let pipe = gpu.cloud_pipe(&device.device, CLOUD_MAP_FORMAT, 7);
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("cloud probe map"),
        });
    draw_pass(&mut encoder, &pipe, &bg, &target.view, "cloud probe map", true);
    submit(device, encoder)?;
    let cells = read_half(device, &target.texture, size, size, 1)?;
    // Toroidal addressing: the map stores cell c at texel c mod size; put the window in order.
    let mut out = vec![1.0_f32; (size * size) as usize];
    for y in 0..size {
        for x in 0..size {
            let cell = crate::clouds::cloud_shadow_texel_cell([x, y], [-half, -half], size);
            let (cx, cy) = ((cell[0] + half) as u32, (cell[1] + half) as u32);
            out[(cy * size + cx) as usize] = cells[(y * size + x) as usize];
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::half_to_f32;

    #[test]
    fn half_floats_decode() {
        assert_eq!(half_to_f32(0), 0.0);
        assert_eq!(half_to_f32(0x3c00), 1.0);
        assert_eq!(half_to_f32(0xc000), -2.0);
        assert_eq!(half_to_f32(0x7bff), 65504.0);
        assert_eq!(half_to_f32(0x0001), (-24.0_f32).exp2());
        assert!(half_to_f32(0x7c01).is_nan());
        assert_eq!(half_to_f32(0x7c00), f32::INFINITY);
    }
}
