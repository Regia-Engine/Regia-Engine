// SPDX-License-Identifier: MPL-2.0
//! GPU tests of the temporal resolve on synthetic HDR frames (v0.1.4 lane `v014-taau-dots`:
//! "white lines/dots around anything when there is movement" on an airless, black-sky map).
//!
//! The camera is orthographic, one world unit per pixel, and pans along +X; the scene depth is a
//! constant plane, so the velocity pass's camera-motion draw writes the exact pan into every pixel.
//! Each frame's current colour is the box-filtered coverage of axis-aligned rectangles at that
//! frame's camera position, uploaded as `Rgba16Float` (render size = output size).

use super::*;
use regia_core::glam::camera::rh::proj::directx::orthographic;
use regia_core::glam::{Mat4, Vec3};

const W: u32 = 96;
const H: u32 = 48;
/// Pan per frame, pixels (the motion-look lane's 2.5 px/frame).
const PAN_PX: f32 = 2.5;

/// An axis-aligned rectangle in screen pixels at camera x = 0, with its HDR value (grey).
#[derive(Clone, Copy)]
struct Rect {
    x0: f32,
    x1: f32,
    y0: f32,
    y1: f32,
    value: f32,
}

/// One texel overridden after the coverage is computed (a firefly, an Inf or a NaN).
#[derive(Clone, Copy)]
struct Inject {
    x: u32,
    y: u32,
    /// Half-float bits written to R, G and B.
    bits: u16,
}

fn overlap(a0: f32, a1: f32, b0: f32, b1: f32) -> f32 {
    (a1.min(b1) - a0.max(b0)).max(0.0)
}

/// Coverage image (luminance = each channel) of `rects` with the camera at `cam_x`.
fn coverage(rects: &[Rect], cam_x: f32) -> Vec<f32> {
    let mut out = vec![0.0f32; (W * H) as usize];
    for y in 0..H {
        for x in 0..W {
            let sx = x as f32 + cam_x;
            let sy = y as f32;
            let mut v = 0.0;
            for r in rects {
                v += r.value * overlap(sx, sx + 1.0, r.x0, r.x1) * overlap(sy, sy + 1.0, r.y0, r.y1);
            }
            out[(y * W + x) as usize] = v;
        }
    }
    out
}

/// IEEE half-float bits of `v` (round to nearest even; NaN, ±Inf and overflow kept).
fn f16_bits(v: f32) -> u16 {
    if v.is_nan() {
        return 0x7e00;
    }
    let b = v.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let a = v.abs();
    if a >= 65520.0 {
        return sign | 0x7c00;
    }
    if a < 6.103_515_6e-5 {
        return sign | (a * 16_777_216.0).round() as u16;
    }
    let e = ((b >> 23) & 0xff) - 112;
    let m = b & 0x7f_ffff;
    let mut h = (e << 10) | (m >> 13);
    let rest = m & 0x1fff;
    if rest > 0x1000 || (rest == 0x1000 && h & 1 == 1) {
        h += 1;
    }
    sign | h as u16
}

fn f16_value(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let e = i32::from((h >> 10) & 0x1f);
    let m = f32::from(h & 0x3ff);
    match e {
        0 => sign * m * 2f32.powi(-24),
        31 if m == 0.0 => sign * f32::INFINITY,
        31 => f32::NAN,
        _ => sign * (1.0 + m / 1024.0) * 2f32.powi(e - 15),
    }
}

fn view_proj(cam_x: f32) -> [[f32; 4]; 4] {
    let (hw, hh) = (W as f32 * 0.5, H as f32 * 0.5);
    (orthographic(-hw, hw, -hh, hh, 0.1, 100.0) * Mat4::from_translation(Vec3::new(-hw - cam_x, hh, 0.0)))
        .to_cols_array_2d()
}

struct Rig {
    device: crate::RenderDevice,
    taau: TaauPass,
    belt: crate::staging::StagingBelt,
    current: wgpu::Texture,
    current_view: wgpu::TextureView,
    depth_view: wgpu::TextureView,
}

impl Rig {
    fn new() -> Option<Self> {
        let device = match crate::RenderDevice::headless() {
            Ok(device) => device,
            Err(err) => {
                eprintln!("skip taau gpu test: {err}");
                return None;
            }
        };
        let d = &device.device;
        let size = wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        };
        let current = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("test current"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let current_view = current.create_view(&wgpu::TextureViewDescriptor::default());
        let depth = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("test depth"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        drop(encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("test depth clear"),
            color_attachments: &[],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(0.5),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        }));
        device.queue.submit([encoder.finish()]);
        Some(Self {
            device,
            taau: TaauPass::default(),
            belt: crate::staging::StagingBelt::default(),
            current,
            current_view,
            depth_view,
        })
    }

    /// Uploads `lum` (grey) with `inject` applied, runs velocity + resolve, returns
    /// (sharpened display, unsharpened resolve) as luminance.
    fn frame(
        &mut self,
        lum: &[f32],
        inject: &[Inject],
        cam_x: f32,
        prev_cam_x: f32,
        first: bool,
    ) -> (Vec<f32>, Vec<f32>) {
        let mut texels = Vec::with_capacity(lum.len() * 4);
        for &v in lum {
            let h = f16_bits(v);
            texels.extend([h, h, h, f16_bits(1.0)]);
        }
        for i in inject {
            let at = ((i.y * W + i.x) * 4) as usize;
            texels[at..at + 3].fill(i.bits);
        }
        self.device.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.current,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&texels),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(W * 8),
                rows_per_image: Some(H),
            },
            wgpu::Extent3d {
                width: W,
                height: H,
                depth_or_array_layers: 1,
            },
        );
        let d = self.device.device.clone();
        self.belt.begin_frame();
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let vp = view_proj(cam_x);
        self.taau
            .record_velocity(
                &d,
                &mut self.belt,
                &mut encoder,
                W,
                H,
                vp,
                vp,
                view_proj(prev_cam_x),
                &[],
                &[],
                &[],
                Some(&self.depth_view),
                Some(&self.depth_view),
            )
            .expect("velocity");
        let display = self
            .taau
            .resolve(
                &d,
                &mut self.belt,
                &mut encoder,
                &self.current_view,
                Some(&self.depth_view),
                W,
                H,
                W,
                H,
                0.9,
                !first,
                MOTION_SHARPEN,
            )
            .expect("resolve");
        let resolved = self.taau.last_output(W, H).expect("resolve output");
        self.belt.upload(&self.device.tracked_queue());
        self.device.queue.submit([encoder.finish()]);
        (self.read(display.texture()), self.read(resolved.texture()))
    }

    /// Luminance of an `Rgba16Float` texture of `W × H` (NaN and Inf kept).
    fn read(&self, texture: &wgpu::Texture) -> Vec<f32> {
        read_rgba(&self.device, texture, W, H)
            .iter()
            .map(|p| 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2])
            .collect()
    }
}

/// The texels of a `w × h` `Rgba16Float` texture (NaN and Inf kept).
fn read_rgba(device: &crate::RenderDevice, texture: &wgpu::Texture, w: u32, h: u32) -> Vec<[f32; 4]> {
    let d = &device.device;
    let row = w * 8;
    let padded = row.div_ceil(256) * 256;
    let staging = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("test readback"),
        size: u64::from(padded * h),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
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
    device
        .tracked()
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    rx.recv().expect("map").expect("map ok");
    let mapped = staging.slice(..).get_mapped_range().expect("range");
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        let start = (y * padded) as usize;
        let halves: &[u16] = bytemuck::cast_slice(&mapped[start..start + row as usize]);
        for p in halves.as_chunks::<4>().0 {
            out.push([f16_value(p[0]), f16_value(p[1]), f16_value(p[2]), f16_value(p[3])]);
        }
    }
    out
}

/// Pans `frames` frames over `rects` (camera x = `PAN_PX` × frame) and returns the last
/// frame's (display, resolve). `TAAU_TEST_DUMP=<dir>` writes every displayed frame as
/// `<tag>_<i>.pgm` (`L / (1 + L)`, gamma 1/2.2: a viewing aid, not the engine's tonemap).
fn pan(rig: &mut Rig, rects: &[Rect], inject: &[Inject], frames: u32, tag: &str) -> (Vec<f32>, Vec<f32>) {
    let dump = regia_core::brand::env_var_os("TAAU_TEST_DUMP").map(std::path::PathBuf::from);
    let mut last = (Vec::new(), Vec::new());
    for f in 0..frames {
        let cam = PAN_PX * f as f32;
        let prev = PAN_PX * f.saturating_sub(1) as f32;
        last = rig.frame(&coverage(rects, cam), inject, cam, prev, f == 0);
        if let Some(dir) = dump.as_ref() {
            let mut pgm = format!("P5\n{W} {H}\n255\n").into_bytes();
            pgm.extend(last.0.iter().map(|&l| {
                let l = if l.is_finite() { l.max(0.0) } else { 1.0e9 };
                ((l / (1.0 + l)).powf(1.0 / 2.2) * 255.0).round() as u8
            }));
            let _ = std::fs::create_dir_all(dir);
            let _ = std::fs::write(dir.join(format!("{tag}_{f:02}.pgm")), pgm);
        }
    }
    last
}

/// Item 1: the motion sharpen must not brighten a silhouette against an exactly-black
/// background (it did ×9 to ×84 in log luminance: white outlines after the tonemap).
#[test]
fn motion_sharpen_does_not_brighten_silhouettes_on_black() {
    let Some(mut rig) = Rig::new() else {
        return;
    };
    // A bright quad and a 1 px bar on an exactly-0 background; 9 frames at 2.5 px/frame end
    // pixel-aligned (camera x = 20), so the native frame's edges are whole pixels.
    let rects = [
        Rect {
            x0: 30.0,
            x1: 50.0,
            y0: 12.0,
            y1: 28.0,
            value: 4.0,
        },
        Rect {
            x0: 64.0,
            x1: 65.0,
            y0: 8.0,
            y1: 40.0,
            value: 4.0,
        },
    ];
    let frames = 9;
    let (display, resolved) = pan(&mut rig, &rects, &[], frames, "sharpen");
    let native = coverage(&rects, PAN_PX * (frames - 1) as f32);
    let native_max = native.iter().copied().fold(0.0f32, f32::max);
    let out_max = display.iter().copied().fold(0.0f32, f32::max);
    assert!(
        out_max <= 1.3 * native_max,
        "sharpened pan peaks at {out_max:.3}, native {native_max:.3} (> 1.3×)"
    );
    // The sharpen's own gain over the resolve it sharpens. The dim, partly covered pixel just
    // outside a moving edge (0.44 next to 4.0 and black) came out 3.0 (×6.9): a white outline.
    let mut gain = (0.0f32, 0u32, 0u32);
    for y in 0..H {
        for x in 0..W {
            let i = (y * W + x) as usize;
            if resolved[i] > 1.0e-3 && display[i] / resolved[i] > gain.0 {
                gain = (display[i] / resolved[i], x, y);
            }
        }
    }
    assert!(
        gain.0 <= 1.3,
        "the motion sharpen brightens ({}, {}) {:.2}× ({:.3} -> {:.3})",
        gain.1,
        gain.2,
        gain.0,
        resolved[(gain.2 * W + gain.1) as usize],
        display[(gain.2 * W + gain.1) as usize]
    );
    let mut worst = (0.0f32, 0u32, 0u32);
    for y in 1..H - 1 {
        for x in 1..W - 1 {
            let i = (y * W + x) as usize;
            let n = native[i];
            let silhouette = n > 0.0
                && [i - 1, i + 1, i - W as usize, i + W as usize]
                    .iter()
                    .any(|&j| native[j] == 0.0);
            if silhouette && display[i] / n > worst.0 {
                worst = (display[i] / n, x, y);
            }
        }
    }
    assert!(
        worst.0 <= 1.3,
        "silhouette pixel ({}, {}) prints {:.2}× native",
        worst.1,
        worst.2,
        worst.0
    );
}

/// Item 2: a mesh moving behind a wall must not write its motion onto the wall. The velocity
/// pass's depth attachment holds draw-order keys, so before the occlusion test the later draw won
/// wherever the two overlapped on screen, visible or not.
fn hidden_mover_keeps_wall_motion(depth_velocity: bool) {
    let device = match crate::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip taau gpu test: {err}");
            return;
        }
    };
    const SW: u32 = 160;
    const SH: u32 = 96;
    const FOV_Y: f32 = 0.9;
    const WALL_DIST: f32 = 7.95;
    const PAN_M: f32 = 0.05;
    let (wv, wi) = crate::box_mesh([1.0, 1.0, 0.05]);
    let wall = crate::LabMesh {
        vertices: crate::translate_mesh(&wv, [0.0, 1.5, 0.0]),
        indices: wi,
        material: 0,
        ..crate::LabMesh::unskinned()
    };
    // Behind the wall (11 m from the eye); its top shows above the wall, the rest is hidden.
    let (cv, ci) = crate::box_mesh([0.8, 0.8, 0.8]);
    let mover = crate::LabMesh {
        vertices: cv,
        indices: ci,
        material: 1,
        ..crate::LabMesh::unskinned()
    };
    let lab = crate::LabScene {
        width: SW,
        height: SH,
        eye: [0.0, 1.5, 8.0],
        target: [0.0, 1.5, 0.0],
        fov_y: FOV_Y,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 14.5,
        ambient_lux: 20_000.0,
        double_sided: false,
        post: None,
        directional: crate::DirectionalLightUniform::new([0.0, -0.3, -1.0], [1.0, 1.0, 1.0], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![
            crate::LabMaterial {
                base_color: [0.5, 0.5, 0.5, 1.0],
                ..crate::LabMaterial::default()
            },
            crate::LabMaterial {
                base_color: [0.8, 0.2, 0.2, 1.0],
                ..crate::LabMaterial::default()
            },
        ],
        meshes: vec![wall, mover],
    };
    let mut quality = crate::FrameQuality::from_profile(&crate::RenderProfile::from_preset(
        crate::RenderProfilePreset::DesktopHigh,
    ));
    quality.screen_space_gi = false;
    quality.ssr = false;
    quality.ssao = crate::SsaoMode::Off;
    quality.gi = crate::GiMode::Ambient;
    quality.reflection_probes = false;
    quality.volumetric_fog = false;
    quality.volumetric_clouds = false;
    quality.physical_atmosphere = false;
    quality.taau = true;
    quality.depth_velocity = depth_velocity;
    let post = crate::PostSettings {
        auto_exposure: false,
        exposure_ev: 14.5,
        bloom: false,
        taa: false,
        ..crate::PostSettings::golden_deterministic()
    };
    let mut frame = lab.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = crate::FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    let mut cam_x = 0.0;
    for f in 0..3 {
        cam_x = PAN_M * f as f32;
        frame.eye = [cam_x, 1.5, 8.0];
        frame.target = [cam_x, 1.5, 0.0];
        let mover_x = -0.3 + 0.3 * f as f32;
        frame.meshes[1].model = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [mover_x, 2.8, -3.0, 1.0],
        ];
        renderer.render_offscreen(&device, &frame).expect("frame");
    }
    let (texture, vw, vh) = renderer
        .taau_pass_for_tests()
        .velocity_texture()
        .expect("velocity target");
    let velocity = read_rgba(&device, texture, vw, vh);
    let tan = (FOV_Y * 0.5).tan();
    let aspect = SW as f32 / SH as f32;
    // The wall's front face on screen (UV), shrunk by 3 velocity pixels.
    let to_uv = |x: f32, y: f32| {
        let d = WALL_DIST;
        (
            0.5 + (x - cam_x) / (2.0 * d * tan * aspect),
            0.5 - (y - 1.5) / (2.0 * d * tan),
        )
    };
    let (u0, v0) = to_uv(-1.0, 2.5);
    let (u1, v1) = to_uv(1.0, 0.5);
    let m = 3.0;
    let x0 = (u0 * vw as f32 + m) as u32;
    let x1 = (u1 * vw as f32 - m) as u32;
    let y0 = (v0 * vh as f32 + m) as u32;
    let y1 = (v1 * vh as f32 - m) as u32;
    // A plane facing the camera moves uniformly: the pan in UV at the wall's distance.
    let expected = PAN_M / (2.0 * WALL_DIST * tan * aspect);
    let mut worst = (0.0f32, 0, 0);
    for y in y0..y1 {
        for x in x0..x1 {
            let v = velocity[(y * vw + x) as usize];
            let err = (v[0] - expected).abs() + v[1].abs();
            if err > worst.0 {
                worst = (err, x, y);
            }
        }
    }
    assert!(
        worst.0 <= 0.15 * expected,
        "depth_velocity {depth_velocity}: wall pixel ({}, {}) holds motion {:?}, camera motion is ({expected:.5}, 0)",
        worst.1,
        worst.2,
        velocity[(worst.2 * vw + worst.1) as usize]
    );
    // Not vacuous: the mover's visible top (above the wall) holds the mover's own motion.
    let mover_motion = velocity[..(y0.saturating_sub(4) * vw) as usize]
        .iter()
        .map(|v| v[0])
        .fold(0.0f32, f32::min);
    assert!(
        mover_motion < -0.5 * expected,
        "depth_velocity {depth_velocity}: the mover's visible part has no motion ({mover_motion:.5})"
    );
}

#[test]
fn hidden_mover_keeps_wall_motion_depth_velocity() {
    hidden_mover_keeps_wall_motion(true);
}

#[test]
fn hidden_mover_keeps_wall_motion_per_mesh_velocity() {
    hidden_mover_keeps_wall_motion(false);
}

/// ADR-1449 (look-2 finding 3): a masked card moving in front of a wall must not write its motion
/// where its texels are alpha-cut. The velocity pass draws without the material, so the whole quad
/// wrote the card's motion; the forward pass shows the wall there, and TAAU fetched the wall's
/// history from where the card moved. The card moves vertically (a scatter tree following the
/// terrain morph) while the camera pans; its cut half covers the right half of the wall.
fn cut_card_keeps_wall_motion(depth_velocity: bool) {
    let device = match crate::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip taau gpu test: {err}");
            return;
        }
    };
    const SW: u32 = 160;
    const SH: u32 = 96;
    const FOV_Y: f32 = 0.9;
    const WALL_DIST: f32 = 7.95;
    const CARD_DIST: f32 = 5.0;
    const PAN_M: f32 = 0.05;
    let (wv, wi) = crate::box_mesh([1.0, 1.0, 0.05]);
    let wall = crate::LabMesh {
        vertices: crate::translate_mesh(&wv, [0.0, 1.5, 0.0]),
        indices: wi,
        material: 0,
        ..crate::LabMesh::unskinned()
    };
    // A quad facing the camera, u = 0 at -x: opaque for u < 0.5, cut for u >= 0.5 (x > 0).
    let corners = [
        ([-1.2f32, -1.2, 0.0], [0.0f32, 1.0]),
        ([1.2, -1.2, 0.0], [1.0, 1.0]),
        ([1.2, 1.2, 0.0], [1.0, 0.0]),
        ([-1.2, 1.2, 0.0], [0.0, 0.0]),
    ];
    let card = crate::LabMesh {
        vertices: corners
            .iter()
            .map(|&(position, uv)| crate::LabVertex::from_pos_normal_uv(position, [0.0, 0.0, 1.0], uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material: 1,
        ..crate::LabMesh::unskinned()
    };
    let mut rgba = Vec::with_capacity(8 * 8 * 4);
    for _row in 0..8 {
        for col in 0..8 {
            rgba.extend_from_slice(&[60, 140, 50, if col < 4 { 255 } else { 0 }]);
        }
    }
    let lab = crate::LabScene {
        width: SW,
        height: SH,
        eye: [0.0, 1.5, 8.0],
        target: [0.0, 1.5, 0.0],
        fov_y: FOV_Y,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 14.5,
        ambient_lux: 20_000.0,
        double_sided: false,
        post: None,
        directional: crate::DirectionalLightUniform::new([0.0, -0.3, -1.0], [1.0, 1.0, 1.0], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![
            crate::LabMaterial {
                base_color: [0.5, 0.5, 0.5, 1.0],
                ..crate::LabMaterial::default()
            },
            crate::LabMaterial {
                base_color: [1.0, 1.0, 1.0, 1.0],
                albedo: Some(crate::LabTexture {
                    width: 8,
                    height: 8,
                    rgba,
                }),
                alpha_mode: crate::ALPHA_MASK,
                alpha_cutoff: 0.5,
                ..crate::LabMaterial::default()
            },
        ],
        meshes: vec![wall, card],
    };
    let mut quality = crate::FrameQuality::from_profile(&crate::RenderProfile::from_preset(
        crate::RenderProfilePreset::DesktopHigh,
    ));
    quality.screen_space_gi = false;
    quality.ssr = false;
    quality.ssao = crate::SsaoMode::Off;
    quality.gi = crate::GiMode::Ambient;
    quality.reflection_probes = false;
    quality.volumetric_fog = false;
    quality.volumetric_clouds = false;
    quality.physical_atmosphere = false;
    quality.taau = true;
    quality.depth_velocity = depth_velocity;
    let post = crate::PostSettings {
        auto_exposure: false,
        exposure_ev: 14.5,
        bloom: false,
        taa: false,
        ..crate::PostSettings::golden_deterministic()
    };
    let mut frame = lab.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = crate::FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    let mut cam_x = 0.0;
    for f in 0..3 {
        cam_x = PAN_M * f as f32;
        frame.eye = [cam_x, 1.5, 8.0];
        frame.target = [cam_x, 1.5, 0.0];
        // 12 cm a frame, upward: about 2.4 velocity pixels, all of it vertical.
        frame.meshes[1].model = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 1.5 + 0.12 * f as f32, 8.0 - CARD_DIST, 1.0],
        ];
        renderer.render_offscreen(&device, &frame).expect("frame");
    }
    let (texture, vw, vh) = renderer
        .taau_pass_for_tests()
        .velocity_texture()
        .expect("velocity target");
    let velocity = read_rgba(&device, texture, vw, vh);
    let tan = (FOV_Y * 0.5).tan();
    let aspect = SW as f32 / SH as f32;
    let to_uv = |x: f32, y: f32, d: f32| {
        (
            0.5 + (x - cam_x) / (2.0 * d * tan * aspect),
            0.5 - (y - 1.5) / (2.0 * d * tan),
        )
    };
    let m = 3.0;
    let px = |u: f32, size: u32, inset: f32| (u * size as f32 + inset) as u32;
    // The wall's right half (x > 0), all of it behind the card's cut half.
    let (u0, v0) = to_uv(0.0, 2.5, WALL_DIST);
    let (u1, v1) = to_uv(1.0, 0.5, WALL_DIST);
    let (x0, x1, y0, y1) = (px(u0, vw, m), px(u1, vw, -m), px(v0, vh, m), px(v1, vh, -m));
    let expected = PAN_M / (2.0 * WALL_DIST * tan * aspect);
    let mut worst = (0.0f32, 0, 0);
    for y in y0..y1 {
        for x in x0..x1 {
            let v = velocity[(y * vw + x) as usize];
            let err = (v[0] - expected).abs() + v[1].abs();
            if err > worst.0 {
                worst = (err, x, y);
            }
        }
    }
    assert!(
        worst.0 <= 0.15 * expected,
        "depth_velocity {depth_velocity}: wall pixel ({}, {}) behind the card's cut half holds motion {:?}, the wall's is ({expected:.5}, 0)",
        worst.1,
        worst.2,
        velocity[(worst.2 * vw + worst.1) as usize]
    );
    // Not vacuous: the card's opaque half (x < 0, in front of the wall's left half) holds the
    // card's own motion, mostly vertical.
    let (cu0, cv0) = to_uv(-1.0, 2.3, WALL_DIST);
    let (cu1, cv1) = to_uv(-0.2, 0.7, WALL_DIST);
    let mut card_motion = 0.0f32;
    for y in px(cv0, vh, m)..px(cv1, vh, -m) {
        for x in px(cu0, vw, m)..px(cu1, vw, -m) {
            card_motion = card_motion.max(velocity[(y * vw + x) as usize][1].abs());
        }
    }
    assert!(
        card_motion > 3.0 * expected,
        "depth_velocity {depth_velocity}: the card's opaque half holds no vertical motion ({card_motion:.5})"
    );
}

#[test]
fn cut_card_keeps_wall_motion_depth_velocity() {
    cut_card_keeps_wall_motion(true);
}

#[test]
fn cut_card_keeps_wall_motion_per_mesh_velocity() {
    cut_card_keeps_wall_motion(false);
}

/// Item 3: one NaN, one +Inf and one 6e4 texel (the forward pass's clamp of a sun glint) in
/// every current frame of a pan. The resolve must stay finite, within the current frame's 3×3
/// range, and the lone 6e4 texel must not print (inverse-luminance weights, Karis 2014).
#[test]
fn resolve_rejects_non_finite_and_firefly_texels() {
    let Some(mut rig) = Rig::new() else {
        return;
    };
    let rects = [
        Rect {
            x0: -100.0,
            x1: 300.0,
            y0: 0.0,
            y1: H as f32,
            value: 0.2,
        },
        Rect {
            x0: 50.0,
            x1: 70.0,
            y0: 10.0,
            y1: 38.0,
            value: 1.8,
        },
    ];
    let firefly = (20u32, 25u32);
    let inject = [
        Inject {
            x: 40,
            y: 20,
            bits: 0x7e00,
        },
        Inject {
            x: 60,
            y: 30,
            bits: 0x7c00,
        },
        Inject {
            x: firefly.0,
            y: firefly.1,
            bits: f16_bits(6.0e4),
        },
    ];
    let frames = 9;
    let (display, resolved) = pan(&mut rig, &rects, &inject, frames, "firefly");
    // The last current frame as uploaded, non-finite texels read as 0.
    let mut current = coverage(&rects, PAN_PX * (frames - 1) as f32);
    for i in &inject {
        let v = f16_value(i.bits);
        current[(i.y * W + i.x) as usize] = if v.is_finite() { v } else { 0.0 };
    }
    let max3 = |x: u32, y: u32| {
        let mut m = 0.0f32;
        for dy in -1i32..=1 {
            for dx in -1i32..=1 {
                let (sx, sy) = (x as i32 + dx, y as i32 + dy);
                if sx >= 0 && sy >= 0 && sx < W as i32 && sy < H as i32 {
                    m = m.max(current[(sy as u32 * W + sx as u32) as usize]);
                }
            }
        }
        m
    };
    // Absolute slack: a bilinear fetch at a texel centre still weighs the next texel by ~1e-6
    // (coordinate rounding), which is 0.03 of a 6e4 neighbour.
    let slack = 0.05;
    for y in 0..H {
        for x in 0..W {
            let i = (y * W + x) as usize;
            assert!(
                resolved[i].is_finite() && display[i].is_finite(),
                "({x}, {y}) is non-finite: resolve {}, display {}",
                resolved[i],
                display[i]
            );
            let bound = max3(x, y);
            assert!(
                resolved[i] <= 1.02 * bound + slack,
                "({x}, {y}) resolves to {:.3}, above the current 3×3 max {bound:.3}",
                resolved[i]
            );
            assert!(
                display[i] <= 1.3 * bound + slack,
                "({x}, {y}) displays {:.3}, above 1.3 × the current 3×3 max {bound:.3}",
                display[i]
            );
        }
    }
    let at = (firefly.1 * W + firefly.0) as usize;
    assert!(
        resolved[at] <= 0.01 * 6.0e4,
        "the lone 6e4 texel resolves to {:.1}: a white dot",
        resolved[at]
    );
}

const SKY_W: u32 = 64;
const SKY_H: u32 = 32;

/// The depth-velocity pass's motion target for a frame of only sky (depth 1 everywhere).
fn sky_velocity(
    device: &crate::RenderDevice,
    depth_view: &wgpu::TextureView,
    cur: Mat4,
    prev: Mat4,
) -> Vec<[f32; 4]> {
    let d = device.device.clone();
    let mut taau = TaauPass::default();
    let mut belt = crate::staging::StagingBelt::default();
    belt.begin_frame();
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    taau.record_velocity(
        &d,
        &mut belt,
        &mut encoder,
        SKY_W,
        SKY_H,
        cur.to_cols_array_2d(),
        cur.to_cols_array_2d(),
        prev.to_cols_array_2d(),
        &[],
        &[],
        &[],
        Some(depth_view),
        Some(depth_view),
    )
    .expect("velocity");
    belt.upload(&device.tracked_queue());
    device.queue.submit([encoder.finish()]);
    let (texture, w, h) = taau.velocity_texture().expect("velocity target");
    assert_eq!((w, h), (SKY_W, SKY_H));
    read_rgba(device, texture, w, h)
}

/// ADR-1151: the depth-velocity pass writes the camera's rotation at sky pixels (depth 1) and none
/// of its translation, because the sky is infinitely far. It used to write zero there: TAAU fetched
/// the sky's history where the sky had been, and stars and a moon's limb faded while the camera
/// turned (the CPU model in `taau_quality.rs` measures 6 % of their brightness kept).
#[test]
fn sky_pixels_carry_the_camera_rotation_and_ignore_its_translation() {
    use regia_core::glam::camera::rh::proj::directx::perspective;
    use regia_core::glam::camera::rh::view::look_at_mat4;
    use regia_core::glam::{Vec2, Vec4};
    let device = match crate::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip taau gpu test: {err}");
            return;
        }
    };
    let d = &device.device;
    let depth = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("test sky depth"),
        size: wgpu::Extent3d {
            width: SKY_W,
            height: SKY_H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Depth32Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    drop(encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("test sky depth clear"),
        color_attachments: &[],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &depth_view,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Clear(1.0),
                store: wgpu::StoreOp::Store,
            }),
            stencil_ops: None,
        }),
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    }));
    device.queue.submit([encoder.finish()]);

    let proj = perspective(1.0, SKY_W as f32 / SKY_H as f32, 0.1, 500.0);
    let view = |eye: Vec3, yaw: f32| look_at_mat4(eye, eye + Vec3::new(yaw.sin(), 0.05, -yaw.cos()), Vec3::Y);
    let uv_of = |clip: Vec4| {
        let ndc = Vec2::new(clip.x, clip.y) / clip.w.abs().max(1.0e-6);
        Vec2::new(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5)
    };
    // The analytic sky motion of pixel (x, y): its view ray as a direction, through both views.
    let expected = |cur: Mat4, prev: Mat4, x: u32, y: u32| {
        let inv = cur.inverse();
        let ndc = Vec2::new(
            (x as f32 + 0.5) / SKY_W as f32 * 2.0 - 1.0,
            1.0 - (y as f32 + 0.5) / SKY_H as f32 * 2.0,
        );
        let near = inv * Vec4::new(ndc.x, ndc.y, 0.0, 1.0);
        let far = inv * Vec4::new(ndc.x, ndc.y, 1.0, 1.0);
        let dir = (far.truncate() / far.w - near.truncate() / near.w).extend(0.0);
        uv_of(prev * dir) - uv_of(cur * dir)
    };

    // A turn of 3° and a 2 m side step between the frames.
    let eye = Vec3::new(4.0, 1.5, -2.0);
    let prev_eye = eye - Vec3::new(2.0, 0.0, 0.5);
    let cur = proj * view(eye, 3.0f32.to_radians());
    let prev = proj * view(prev_eye, 0.0);
    let velocity = sky_velocity(&device, &depth_view, cur, prev);
    let (mut worst, mut largest) = (0.0f32, 0.0f32);
    for y in 0..SKY_H {
        for x in 0..SKY_W {
            let v = velocity[(y * SKY_W + x) as usize];
            let want = expected(cur, prev, x, y);
            worst = worst.max((Vec2::new(v[0], v[1]) - want).length());
            largest = largest.max(want.length());
            // No surface: TSR and FSR 3 take their own camera motion where z is 0.
            assert!(v[2] == 0.0 && v[3] == 0.0, "sky ({x}, {y}) wrote z/alpha {v:?}");
        }
    }
    assert!(largest > 0.02, "the turn should move the sky ({largest} uv)");
    assert!(
        worst < 2.0e-3,
        "sky motion is off the camera rotation by {worst} uv (largest motion {largest})"
    );

    // A pure side step leaves the sky where it was (a far-plane point would move 0.5 % of the
    // view per metre at this 500 m far plane).
    let still = sky_velocity(
        &device,
        &depth_view,
        proj * view(eye, 0.0),
        proj * view(eye - Vec3::new(5.0, 0.0, 0.0), 0.0),
    );
    let drift = still
        .iter()
        .map(|v| v[0].abs().max(v[1].abs()))
        .fold(0.0f32, f32::max);
    assert!(drift < 1.0e-4, "a side step moved the sky by {drift} uv");

    // Orthographic views have no view ray direction: the sky keeps zero motion, as before.
    let ortho = orthographic(-8.0, 8.0, -4.0, 4.0, 0.1, 500.0);
    let flat = sky_velocity(
        &device,
        &depth_view,
        ortho * view(eye, 3.0f32.to_radians()),
        ortho * view(prev_eye, 0.0),
    );
    assert!(
        flat.iter().all(|v| v[0] == 0.0 && v[1] == 0.0),
        "an orthographic sky wrote motion"
    );
}

/// The GPU resolve against the CPU model of `taau_quality.rs` (ADR-1151) on one moving-camera
/// sequence with parallax, `FRAMES` frames: the camera steps sideways, a textured wall moves
/// 1 px/frame and a mid-gray box in front of it 3 px/frame. Color and depth are the model's own
/// unjittered frames; the motion comes from the real depth-velocity pass (perspective matrices
/// and a depth target), so the test also checks that pass against the model's layer motion.
fn gpu_resolve_matches_the_cpu_model(render_w: u32, render_h: u32) {
    use crate::taau_quality::{Coverage, Pattern, ResolveTuning, TemporalLayer, TemporalScene};
    use regia_core::glam::Vec4;
    use regia_core::glam::camera::rh::proj::directx::perspective;
    use regia_core::glam::camera::rh::view::look_at_mat4;
    const OW: u32 = 96;
    const OH: u32 = 48;
    const FRAMES: u32 = 12;
    const FOV_Y: f32 = 0.9;
    const STEP_M: f32 = 0.1;
    let device = match crate::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip taau gpu test: {err}");
            return;
        }
    };
    let proj = perspective(FOV_Y, OW as f32 / OH as f32, 0.5, 50.0);
    let focal_px = OH as f32 * 0.5 / (FOV_Y * 0.5).tan();
    // View distances that move 1 px (wall) and 3 px (box) per frame for the step.
    let (wall_z, box_z) = (focal_px * STEP_M, focal_px * STEP_M / 3.0);
    let ndc_depth = |z: f32| {
        let clip = proj * Vec4::new(0.0, 0.0, -z, 1.0);
        clip.z / clip.w
    };
    let layer = |z: f32, pattern, coverage, start, foreground| TemporalLayer {
        depth: ndc_depth(z),
        pattern,
        coverage,
        start,
        velocity: [-focal_px * STEP_M / z, 0.0],
        foreground,
    };
    let scene = TemporalScene {
        name: "gpu parity".into(),
        layers: vec![
            layer(
                wall_z,
                Pattern::Waves {
                    period: 9.0,
                    lo: 0.1,
                    hi: 0.9,
                },
                Coverage::Everywhere,
                [0.0, 0.0],
                false,
            ),
            layer(
                box_z,
                Pattern::Flat([0.5, 0.45, 0.4]),
                Coverage::Rect {
                    width: 14.0,
                    height: 18.0,
                },
                [70.0, 15.0],
                true,
            ),
        ],
    };
    let (rw, rh) = (render_w as usize, render_h as usize);
    let output = (OW as usize, OH as usize);
    let wide = scene.unjittered_frames((rw, rh), output, FRAMES, &ResolveTuning::SHIPPED);
    let narrow = scene.unjittered_frames(
        (rw, rh),
        output,
        FRAMES,
        &ResolveTuning {
            wide_motion_test: false,
            ..ResolveTuning::SHIPPED
        },
    );

    let d = &device.device;
    let size = wgpu::Extent3d {
        width: render_w,
        height: render_h,
        depth_or_array_layers: 1,
    };
    let texture = |label, format| {
        d.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    };
    // Depth16Unorm: a depth format the queue can write (Depth32Float cannot be a copy destination).
    let current = texture("parity current", wgpu::TextureFormat::Rgba16Float);
    let depth = texture("parity depth", wgpu::TextureFormat::Depth16Unorm);
    let current_view = current.create_view(&wgpu::TextureViewDescriptor::default());
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
    let upload = |target: &wgpu::Texture, bytes: &[u8], texel_bytes: u32| {
        device.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(render_w * texel_bytes),
                rows_per_image: Some(render_h),
            },
            size,
        );
    };
    let view_proj = |frame: f32| {
        let eye = Vec3::new(STEP_M * frame, 0.0, 0.0);
        (proj * look_at_mat4(eye, eye - Vec3::Z, Vec3::Y)).to_cols_array_2d()
    };
    let luma = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    let mut taau = TaauPass::default();
    let mut belt = crate::staging::StagingBelt::default();
    let (mut sum, mut count, mut far_off) = (0.0f64, 0usize, 0usize);
    let (mut split, mut toward_wide) = (0usize, 0usize);
    for (f, (model, old)) in wide.iter().zip(&narrow).enumerate() {
        let colour: Vec<u16> = model
            .colour
            .iter()
            .flat_map(|c| [f16_bits(c.x), f16_bits(c.y), f16_bits(c.z), f16_bits(1.0)])
            .collect();
        let depth_bits: Vec<u16> = model
            .depth
            .iter()
            .map(|&z| (z.clamp(0.0, 1.0) * 65535.0).round() as u16)
            .collect();
        upload(&current, bytemuck::cast_slice(&colour), 8);
        upload(&depth, bytemuck::cast_slice(&depth_bits), 2);
        belt.begin_frame();
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let (cur, prev) = (view_proj(f as f32), view_proj(f as f32 - 1.0));
        taau.record_velocity(
            d,
            &mut belt,
            &mut encoder,
            render_w,
            render_h,
            cur,
            cur,
            prev,
            &[],
            &[],
            &[],
            Some(&depth_view),
            Some(&depth_view),
        )
        .expect("velocity");
        let out = taau
            .resolve(
                d,
                &mut belt,
                &mut encoder,
                &current_view,
                Some(&depth_view),
                render_w,
                render_h,
                OW,
                OH,
                0.9,
                f > 0,
                0.0,
            )
            .expect("resolve");
        belt.upload(&device.tracked_queue());
        device.queue.submit([encoder.finish()]);
        let gpu = read_rgba(&device, out.texture(), OW, OH);
        for ((g, m), n) in gpu.iter().zip(&model.out).zip(&old.out) {
            let lg = luma([g[0], g[1], g[2]]);
            let (lm, ln) = (luma(m.to_array()), luma(n.to_array()));
            let diff = (lg - lm).abs();
            sum += f64::from(diff);
            count += 1;
            if diff > 0.1 {
                far_off += 1;
            }
            // Where the wide and the nearest-texel motion tests disagree (174 pixels at 0.67 in
            // the numpy port of the model, none at native scale), the GPU must side with the wide
            // one (the shader's upscaled path). Texel picks at exact boundaries can flip between
            // the GPU and the model, so three in four is the bar.
            if (lm - ln).abs() > 0.05 {
                split += 1;
                if diff < (lg - ln).abs() {
                    toward_wide += 1;
                }
            }
        }
    }
    let mean = sum / count as f64;
    eprintln!(
        "render {render_w}x{render_h}: mean |gpu - model| {mean:.4}, {far_off} of {count} pixels off by > 0.1, \
         {toward_wide} of {split} split pixels side with the wide test"
    );
    assert!(
        mean < 0.01,
        "the GPU resolve is {mean:.4} from the CPU model on average"
    );
    assert!(
        far_off * 100 < count,
        "{far_off} of {count} pixels differ from the CPU model by more than 0.1"
    );
    if render_w < OW {
        assert!(
            split > 0,
            "the 0.67 sequence should exercise the wide motion test"
        );
        assert!(
            toward_wide * 4 >= split * 3,
            "only {toward_wide} of {split} pixels where the motion tests differ follow the wide test"
        );
    } else {
        assert_eq!(split, 0, "at native scale both motion tests are the same");
    }
}

#[test]
fn gpu_resolve_matches_the_cpu_model_at_native_scale() {
    gpu_resolve_matches_the_cpu_model(96, 48);
}

#[test]
fn gpu_resolve_matches_the_cpu_model_upscaling_from_067() {
    gpu_resolve_matches_the_cpu_model(64, 32);
}

#[test]
fn a_steady_velocity_pack_is_not_allocated_again() {
    let Some(mut rig) = Rig::new() else {
        return;
    };
    let instance = VelocityInstance {
        model: Mat4::IDENTITY.to_cols_array_2d(),
        prev_model: Mat4::IDENTITY.to_cols_array_2d(),
        skin: None,
        order: 0,
    };
    let d = rig.device.device.clone();
    let record = |rig: &mut Rig| {
        rig.belt.begin_frame();
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let vp = view_proj(0.0);
        rig.taau
            .record_velocity(
                &d,
                &mut rig.belt,
                &mut encoder,
                W,
                H,
                vp,
                vp,
                vp,
                &[instance],
                &[],
                &[],
                Some(&rig.depth_view),
                None,
            )
            .expect("velocity");
        rig.belt.upload(&rig.device.tracked_queue());
        rig.device.queue.submit([encoder.finish()]);
    };
    record(&mut rig);
    assert_eq!(rig.taau.velocity_pack_allocs, 1, "the first frame packs once");
    assert!(
        rig.taau.velocity_last_written > 0,
        "the first frame uploads the objects"
    );
    record(&mut rig);
    assert_eq!(
        rig.taau.velocity_last_written, 0,
        "identical matrices must write no object bytes"
    );
    assert_eq!(
        rig.taau.velocity_pack_allocs, 1,
        "the second frame allocated another velocity pack"
    );
}
