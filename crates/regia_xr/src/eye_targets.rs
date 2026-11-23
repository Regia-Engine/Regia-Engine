// SPDX-License-Identifier: MPL-2.0
//! Where eye images are rendered (spec 58 § 3.2). The real OpenXR binding wraps the runtime's
//! swapchain images; the simulator's [`SimEyeTargets`] are ordinary wgpu textures, so the renderer's
//! two-eye path and `xr.capture_eye` work with no XR device at all.

use regia_app::prelude::*;
use regia_core::glam::UVec2;
use regia_platform::xr::{Eye, FramePhase, XrError};
use regia_render::RenderDevice;

/// Where eye images are rendered. Lives here, not in `regia_platform::xr`, because it names wgpu.
pub trait EyeTargets: Send {
    /// The per-eye size: the runtime's recommended size times the render scale.
    fn extent(&self) -> UVec2;

    /// The view to render `eye` into this frame (a swapchain backend waits for the image here).
    /// Acquiring an eye twice without releasing it is [`XrError::FrameOutOfOrder`].
    fn acquire(&mut self, eye: Eye) -> Result<wgpu::TextureView, XrError>;

    /// Hands `eye`'s image back for composition. Releasing an eye that was not acquired is
    /// [`XrError::FrameOutOfOrder`].
    fn release(&mut self, eye: Eye) -> Result<(), XrError>;

    /// The image behind `eye`, when a copy can reach it. The multiview strategy draws a two-layer
    /// target and copies each layer here.
    fn image(&self, _eye: Eye) -> Option<wgpu::Texture> {
        None
    }
}

/// The format the simulator's eye targets use (8-bit sRGB, the first in the spec's preference
/// order).
pub const SIM_EYE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Largest simulator eye size per axis.
pub const MAX_SIM_EYE_EXTENT: u32 = 4096;

/// Acquire/release bookkeeping per eye (shared by every [`EyeTargets`] implementation).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AcquireTracker {
    acquired: [bool; 2],
    releases: u64,
}

impl AcquireTracker {
    /// Records an acquire of `eye`.
    pub fn acquire(&mut self, eye: Eye) -> Result<(), XrError> {
        if self.acquired[eye.index()] {
            return Err(XrError::FrameOutOfOrder {
                call: "acquire_eye",
                phase: FramePhase::Begun,
            });
        }
        self.acquired[eye.index()] = true;
        Ok(())
    }

    /// Records a release of `eye`.
    pub fn release(&mut self, eye: Eye) -> Result<(), XrError> {
        if !self.acquired[eye.index()] {
            return Err(XrError::FrameOutOfOrder {
                call: "release_eye",
                phase: FramePhase::Idle,
            });
        }
        self.acquired[eye.index()] = false;
        self.releases += 1;
        Ok(())
    }

    /// Whether `eye` is acquired.
    pub fn is_acquired(&self, eye: Eye) -> bool {
        self.acquired[eye.index()]
    }

    /// Releases so far.
    pub fn releases(&self) -> u64 {
        self.releases
    }
}

/// Two ordinary wgpu textures standing in for the runtime's swapchain images. Insert one as a
/// resource and `xr.capture_eye` reads its images back.
#[derive(Resource)]
pub struct SimEyeTargets {
    device: RenderDevice,
    extent: UVec2,
    textures: [wgpu::Texture; 2],
    views: [wgpu::TextureView; 2],
    tracker: AcquireTracker,
}

impl SimEyeTargets {
    /// Two `extent`-sized [`SIM_EYE_FORMAT`] targets on `device`. Each axis must be 1 to
    /// [`MAX_SIM_EYE_EXTENT`].
    pub fn new(device: &RenderDevice, extent: UVec2) -> Result<Self, XrError> {
        let ok = |v: u32| (1..=MAX_SIM_EYE_EXTENT).contains(&v);
        if !(ok(extent.x) && ok(extent.y)) {
            return Err(XrError::InvalidValue {
                key: "extent".into(),
                reason: format!("each axis must be 1..={MAX_SIM_EYE_EXTENT}"),
            });
        }
        let make = |label: &str| {
            device.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: extent.x,
                    height: extent.y,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: SIM_EYE_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::COPY_SRC
                    | wgpu::TextureUsages::COPY_DST
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
        };
        let textures = [make("xr sim eye 0"), make("xr sim eye 1")];
        let views = [0, 1].map(|index| {
            crate::perf::texture_view();
            textures[index].create_view(&wgpu::TextureViewDescriptor::default())
        });
        Ok(SimEyeTargets {
            device: device.clone(),
            extent,
            textures,
            views,
            tracker: AcquireTracker::default(),
        })
    }

    /// The texture of `eye`.
    pub fn texture(&self, eye: Eye) -> &wgpu::Texture {
        &self.textures[eye.index()]
    }

    /// Reads `eye` back as tightly packed RGBA8 rows (`extent.x * 4` bytes each).
    pub fn read_rgba8(&self, eye: Eye) -> Result<Vec<u8>, XrError> {
        let _span = tracing::debug_span!("xr_capture_eye", eye = eye.index()).entered();
        read_rgba8(&self.device, self.texture(eye), self.extent)
    }
}

impl EyeTargets for SimEyeTargets {
    fn extent(&self) -> UVec2 {
        self.extent
    }

    fn acquire(&mut self, eye: Eye) -> Result<wgpu::TextureView, XrError> {
        self.tracker.acquire(eye)?;
        Ok(self.views[eye.index()].clone())
    }

    fn release(&mut self, eye: Eye) -> Result<(), XrError> {
        self.tracker.release(eye)
    }

    fn image(&self, eye: Eye) -> Option<wgpu::Texture> {
        Some(self.textures[eye.index()].clone())
    }
}

/// Resizes tightly packed RGBA8 `src` (`w × h`) to `nw × nh` with a box filter: each output pixel
/// is the area-weighted mean of the source pixels its footprint covers (integer arithmetic in
/// 1/`nw`·1/`nh` units, so the result is exact and deterministic). Sizes must be non-zero and
/// `src` must hold `w * h * 4` bytes, else `None`.
pub fn box_resize(src: &[u8], w: u32, h: u32, nw: u32, nh: u32) -> Option<Vec<u8>> {
    if w == 0 || h == 0 || nw == 0 || nh == 0 {
        return None;
    }
    let src_len = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
    if src.len() != src_len {
        return None;
    }
    let out_len = (nw as usize).checked_mul(nh as usize)?.checked_mul(4)?;
    let (w, h, nw, nh) = (u64::from(w), u64::from(h), u64::from(nw), u64::from(nh));
    // Weight of source column `sx` in output column `dx`: overlap of [sx*nw, (sx+1)*nw) with
    // [dx*w, (dx+1)*w) in units where one source pixel is `nw` wide.
    let overlap = |s: u64, d: u64, src_n: u64, dst_n: u64| -> u64 {
        let (a0, a1) = (s * dst_n, (s + 1) * dst_n);
        let (b0, b1) = (d * src_n, (d + 1) * src_n);
        a1.min(b1).saturating_sub(a0.max(b0))
    };
    let mut out = Vec::new();
    out.try_reserve_exact(out_len).ok()?;
    for dy in 0..nh {
        let y0 = dy * h / nh;
        let y1 = ((dy + 1) * h).div_ceil(nh).min(h);
        for dx in 0..nw {
            let x0 = dx * w / nw;
            let x1 = ((dx + 1) * w).div_ceil(nw).min(w);
            let mut acc = [0u64; 4];
            let mut total = 0u64;
            for sy in y0..y1 {
                let wy = overlap(sy, dy, h, nh);
                for sx in x0..x1 {
                    let wgt = wy * overlap(sx, dx, w, nw);
                    let i = ((sy * w + sx) * 4) as usize;
                    for (c, a) in acc.iter_mut().enumerate() {
                        *a += u64::from(src[i + c]) * wgt;
                    }
                    total += wgt;
                }
            }
            for a in acc {
                out.push(((a + total / 2) / total.max(1)) as u8);
            }
        }
    }
    Some(out)
}

fn gpu_error(what: impl std::fmt::Display) -> XrError {
    XrError::GpuInteropUnavailable {
        reason: what.to_string(),
    }
}

/// Bytes per tight row and per 256-aligned padded row for an RGBA8 readback of `extent`, or
/// `None` when either size (or the staging buffer total) does not fit.
fn readback_layout(extent: UVec2) -> Option<(u32, u32)> {
    let row = extent.x.checked_mul(4)?;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = row.checked_add(align - 1)? / align * align;
    // The staging buffer (padded * h) and the tight output (row * h) must fit in a u32 index.
    padded.checked_mul(extent.y)?;
    Some((row, padded))
}

/// Copies an RGBA8 `texture` of `extent` to the CPU (tightly packed rows).
pub fn read_rgba8(device: &RenderDevice, texture: &wgpu::Texture, extent: UVec2) -> Result<Vec<u8>, XrError> {
    let (w, h) = (extent.x, extent.y);
    let size = texture.size();
    if extent.x == 0
        || extent.y == 0
        || size.width != w
        || size.height != h
        || size.depth_or_array_layers != 1
    {
        return Err(gpu_error(format!(
            "readback extent {w}x{h} does not match the {}x{}x{} texture",
            size.width, size.height, size.depth_or_array_layers
        )));
    }
    let (row, padded) =
        readback_layout(extent).ok_or_else(|| gpu_error(format!("readback extent {w}x{h} is too large")))?;
    let d = &device.device;
    let staging = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("xr eye readback"),
        size: u64::from(padded) * u64::from(h),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("xr eye readback"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
    device.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    d.poll(wgpu::PollType::wait_indefinitely()).map_err(gpu_error)?;
    rx.recv().map_err(gpu_error)?.map_err(gpu_error)?;
    let mapped = staging.slice(..).get_mapped_range().map_err(gpu_error)?;
    let mut out = Vec::with_capacity((row * h) as usize);
    for y in 0..h {
        let start = (y * padded) as usize;
        out.extend_from_slice(&mapped[start..start + row as usize]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_and_release_must_pair_per_eye() {
        let mut t = AcquireTracker::default();
        t.acquire(Eye::Left).expect("acquire");
        assert_eq!(
            t.acquire(Eye::Left).expect_err("twice").code(),
            "xr_frame_out_of_order"
        );
        t.acquire(Eye::Right).expect("other eye");
        t.release(Eye::Left).expect("release");
        assert_eq!(
            t.release(Eye::Left).expect_err("twice").code(),
            "xr_frame_out_of_order"
        );
        t.release(Eye::Right).expect("release");
        assert_eq!(t.releases(), 2);
        assert!(!t.is_acquired(Eye::Left));
    }

    #[test]
    fn box_resize_rejects_overflowing_sizes_without_panicking() {
        // 2^31 * 2^31 * 4 wraps to 0 on 64-bit: an empty slice must not pass the length check.
        assert_eq!(box_resize(&[], 1 << 31, 1 << 31, 1, 1), None);
        assert_eq!(box_resize(&[0; 4], 1, 1, u32::MAX, u32::MAX), None);
    }

    #[test]
    fn readback_layout_rejects_overflow_and_aligns_rows() {
        assert_eq!(readback_layout(UVec2::new(8, 4)), Some((32, 256)));
        assert_eq!(readback_layout(UVec2::new(1_073_741_823, 1)), None);
        assert_eq!(readback_layout(UVec2::new(1 << 30, 1)), None);
        assert_eq!(readback_layout(UVec2::new(64, u32::MAX)), None);
    }

    #[test]
    fn box_resize_averages_footprints_exactly() {
        // 2x1 red|blue -> 1x1 purple; 1x1 -> 2x2 replicates; identity size is unchanged.
        let src = [255, 0, 0, 255, 0, 0, 255, 255];
        assert_eq!(box_resize(&src, 2, 1, 1, 1), Some(vec![128, 0, 128, 255]));
        assert_eq!(
            box_resize(&src[..4], 1, 1, 2, 2),
            Some([255, 0, 0, 255].repeat(4))
        );
        assert_eq!(box_resize(&src, 2, 1, 2, 1).as_deref(), Some(&src[..]));
        // 3 -> 2: output 0 covers source 0 fully and half of source 1.
        let row = [0, 0, 0, 0, 90, 90, 90, 90, 180, 180, 180, 180];
        assert_eq!(
            box_resize(&row, 3, 1, 2, 1),
            Some(vec![30, 30, 30, 30, 150, 150, 150, 150])
        );
        assert_eq!(box_resize(&src, 2, 1, 0, 1), None);
        assert_eq!(box_resize(&src, 3, 1, 1, 1), None, "wrong length");
    }

    /// GPU (lavapipe is fine): each eye clears to its own color and reads back. Skipped, with a
    /// note, when no adapter exists.
    #[test]
    fn sim_eye_targets_render_and_read_back_per_eye() {
        let Ok(device) = RenderDevice::shared_headless() else {
            eprintln!("skipped: no GPU adapter");
            return;
        };
        let mut targets = SimEyeTargets::new(&device, UVec2::new(8, 4)).expect("targets");
        assert!(SimEyeTargets::new(&device, UVec2::new(0, 4)).is_err());
        let colors = [wgpu::Color::RED, wgpu::Color::BLUE];
        for eye in Eye::BOTH {
            let view = targets.acquire(eye).expect("acquire");
            let mut encoder = device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            {
                let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("xr eye clear"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(colors[eye.index()]),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
            }
            device.queue.submit([encoder.finish()]);
            targets.release(eye).expect("release");
        }
        let left = targets.read_rgba8(Eye::Left).expect("read");
        let right = targets.read_rgba8(Eye::Right).expect("read");
        assert_eq!(left.len(), 8 * 4 * 4);
        assert_eq!(&left[..4], &[255, 0, 0, 255]);
        assert_eq!(&right[..4], &[0, 0, 255, 255]);
    }
}
