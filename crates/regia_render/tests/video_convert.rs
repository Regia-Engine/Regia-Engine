// SPDX-License-Identifier: MPL-2.0
//! The GPU video convert pass (specs/50 § 2.7 and § 8 test 10, ADR-2044) on the lavapipe reference:
//! the compute shader matches the CPU fast path within ±1 for 4:2:0, 4:2:2 and 4:4:4 at 8 and 10
//! bits, limited and full range; the frame pass is off until enabled, converts inside the frame,
//! leaves the frame's own pixels untouched, and keeps one pending picture per player; a material's
//! `video` parameter shows the latest written frame.

use std::sync::Arc;

use regia_core::glam::Mat4;
use regia_render::video::yuv_to_rgba8;
use regia_render::{
    DirectionalLightUniform, DynamicTexture, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene,
    LabMaterial, LabVertex, OwnedYuv, RenderDevice, TrackedDevice, VIDEO_CONVERT_PASS_NAME, VideoConvertJobs,
    VideoConverter, VideoSlot, VideoTarget, YuvMatrix, YuvPlanes, video_material,
};

static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> RenderDevice {
    RenderDevice::headless().expect("headless device (lavapipe)")
}

/// A deterministic picture: a luma ramp with a hashed texture and chroma sweeps.
fn picture(
    w: u32,
    h: u32,
    depth: u8,
    shift: (u32, u32),
    matrix: YuvMatrix,
    full: bool,
    seed: u32,
) -> OwnedYuv {
    let sb = if depth > 8 { 2 } else { 1 };
    let max = (1u32 << depth) - 1;
    let (cw, ch) = (w.div_ceil(1 << shift.0), h.div_ceil(1 << shift.1));
    let plane = |pw: u32, ph: u32, k: u32| -> Arc<[u8]> {
        let mut out = Vec::with_capacity((pw * ph) as usize * sb);
        for y in 0..ph {
            for x in 0..pw {
                let mut v =
                    x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503) ^ k.wrapping_mul(97) ^ seed;
                v ^= v >> 13;
                v = v.wrapping_mul(0x5bd1_e995);
                v ^= v >> 15;
                let s = v % (max + 1);
                if sb == 2 {
                    out.extend_from_slice(&(s as u16).to_le_bytes());
                } else {
                    out.push(s as u8);
                }
            }
        }
        out.into()
    };
    OwnedYuv {
        width: w,
        height: h,
        bit_depth: depth,
        chroma_shift: shift,
        matrix,
        full_range: full,
        planes: [plane(w, h, 0), plane(cw, ch, 1), plane(cw, ch, 2)],
    }
}

fn read_rgba(device: &RenderDevice, texture: &wgpu::Texture) -> Vec<u8> {
    let (w, h) = (texture.width(), texture.height());
    let row = (w * 4).div_ceil(256) * 256;
    let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("video readback"),
        size: u64::from(row * h),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
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
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll readback");
    rx.recv().expect("map callback").expect("map readback");
    let mapped = buffer.slice(..).get_mapped_range().expect("mapped range");
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h as usize {
        let start = y * row as usize;
        out.extend_from_slice(&mapped[start..start + (w * 4) as usize]);
    }
    drop(mapped);
    buffer.unmap();
    out
}

fn max_diff(a: &[u8], b: &[u8]) -> u8 {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0)
}

/// A short picture of a new size must not destroy the player's last good frame: the target is
/// only replaced once the picture proves convertible.
#[test]
fn a_short_picture_of_a_new_size_keeps_the_last_good_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let tracked = TrackedDevice::observe(&device.device);
    let jobs = VideoConvertJobs::default();
    jobs.submit(3, picture(64, 64, 8, (1, 1), YuvMatrix::Bt709, false, 5));
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    let write = |b: &wgpu::Buffer, o: u64, d: &[u8]| device.queue.write_buffer(b, o, d);
    assert_eq!(jobs.record_pending(&tracked, &write, &mut encoder), 1);
    device.queue.submit([encoder.finish()]);
    assert!(jobs.view(3).is_some_and(|(serial, _)| serial == 1));

    // A new size with planes shorter than that size: refused, and the old target survives.
    let mut small = picture(32, 32, 8, (1, 1), YuvMatrix::Bt709, false, 6);
    small.planes[0] = Arc::from(vec![0u8; 3]);
    jobs.submit(3, small);
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    assert_eq!(jobs.record_pending(&tracked, &write, &mut encoder), 0);
    let stats = jobs.stats();
    assert_eq!(stats.failed, 1);
    assert!(
        jobs.view(3).is_some_and(|(serial, _)| serial == 1),
        "the last good frame is still served"
    );
    // A well-formed picture of the new size converts again (the target is rebuilt for the new
    // size; its serial restarts at 1).
    jobs.submit(3, picture(32, 32, 8, (1, 1), YuvMatrix::Bt709, false, 7));
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    assert_eq!(jobs.record_pending(&tracked, &write, &mut encoder), 1);
    let texture = jobs.texture(3).expect("converted texture");
    assert_eq!((texture.width(), texture.height()), (32, 32));
    assert!(jobs.view(3).is_some_and(|(serial, _)| serial == 1));
}

/// specs/50 § 8 test 10, GPU half: the shader agrees with the CPU path within ±1 per channel.
#[test]
fn gpu_convert_matches_the_cpu_path_within_one() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let tracked = TrackedDevice::observe(&device.device);
    let converter = VideoConverter::new(&tracked);
    let cases = [
        (37, 21, 8, (1, 1), YuvMatrix::Bt709, false),
        (33, 17, 10, (1, 0), YuvMatrix::Bt601, false),
        (16, 9, 8, (0, 0), YuvMatrix::Bt2020Ncl, true),
        (65, 33, 10, (1, 1), YuvMatrix::Bt2020Ncl, false),
        (8, 8, 12, (0, 0), YuvMatrix::Bt709, true),
    ];
    for (i, (w, h, depth, shift, matrix, full)) in cases.into_iter().enumerate() {
        let pic = picture(w, h, depth, shift, matrix, full, i as u32);
        let mut target = VideoTarget::new(&tracked, w, h);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let write = |b: &wgpu::Buffer, o: u64, d: &[u8]| device.queue.write_buffer(b, o, d);
        converter
            .record(&tracked, &write, &mut encoder, &pic.planes(), &mut target)
            .expect("record");
        device.queue.submit([encoder.finish()]);
        assert_eq!(target.serial(), 1);
        let gpu = read_rgba(&device, target.front_texture());
        let mut cpu = Vec::new();
        assert!(yuv_to_rgba8(&pic.planes(), &mut cpu));
        let d = max_diff(&gpu, &cpu);
        assert!(
            d <= 1,
            "case {i}: {w}x{h} {depth}-bit {shift:?} {matrix:?} full={full}: max diff {d}"
        );
        eprintln!("video convert case {i}: {w}x{h} {depth}-bit max diff {d}");
    }
}

/// The convert writes the back texture and swaps: the previous front keeps the previous picture.
#[test]
fn the_target_is_double_buffered() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let tracked = TrackedDevice::observe(&device.device);
    let converter = VideoConverter::new(&tracked);
    let a = picture(24, 16, 8, (1, 1), YuvMatrix::Bt709, false, 1);
    let b = picture(24, 16, 8, (1, 1), YuvMatrix::Bt709, false, 2);
    let mut target = VideoTarget::new(&tracked, 24, 16);
    let write = |buf: &wgpu::Buffer, o: u64, d: &[u8]| device.queue.write_buffer(buf, o, d);
    let convert = |pic: &OwnedYuv, target: &mut VideoTarget| {
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        converter
            .record(&tracked, &write, &mut encoder, &pic.planes(), target)
            .expect("record");
        device.queue.submit([encoder.finish()]);
        target.front_texture().clone()
    };
    let first = convert(&a, &mut target);
    let second = convert(&b, &mut target);
    assert!(first != second, "a convert writes the other texture");
    let (mut cpu_a, mut cpu_b) = (Vec::new(), Vec::new());
    assert!(yuv_to_rgba8(&a.planes(), &mut cpu_a) && yuv_to_rgba8(&b.planes(), &mut cpu_b));
    assert!(
        max_diff(&read_rgba(&device, &first), &cpu_a) <= 1,
        "the old front still holds a"
    );
    assert!(
        max_diff(&read_rgba(&device, &second), &cpu_b) <= 1,
        "the new front holds b"
    );
    // A size change is refused rather than written out of bounds.
    let small = picture(8, 8, 8, (1, 1), YuvMatrix::Bt709, false, 3);
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    assert!(
        converter
            .record(&tracked, &write, &mut encoder, &small.planes(), &mut target)
            .is_err()
    );
    let short = OwnedYuv {
        planes: [
            Arc::from(vec![0u8; 3]),
            small.planes[1].clone(),
            small.planes[2].clone(),
        ],
        ..small
    };
    let mut target8 = VideoTarget::new(&tracked, 8, 8);
    assert!(
        converter
            .record(&tracked, &write, &mut encoder, &short.planes(), &mut target8)
            .is_err()
    );
}

fn quad() -> Vec<LabVertex> {
    [[-2.0f32, -2.0], [2.0, -2.0], [2.0, 2.0], [-2.0, 2.0]]
        .into_iter()
        .map(|[x, z]| LabVertex {
            position: [x, 0.0, z],
            normal: [0.0, 1.0, 0.0],
            uv: [(x + 2.0) / 4.0, (z + 2.0) / 4.0],
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect()
}

fn render(renderer: &mut FrameRenderer, device: &RenderDevice, material: &LabMaterial) -> Vec<u8> {
    let vertices = quad();
    let indices = [0u32, 2, 1, 0, 3, 2];
    let scene = FrameScene {
        width: 64,
        height: 64,
        eye: [0.0, 3.0, 0.01],
        target: [0.0, 0.0, 0.0],
        fov_y: 1.1,
        near: 0.05,
        far: 50.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: None,
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0, 1.0, 1.0], 0.0),
        local_lights: &[],
        quality: FrameQuality::LAB,
        meshes: vec![FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: 1,
            object_key: 1,
            vertices: &vertices,
            indices: &indices,
            material,
            model: Mat4::IDENTITY.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: regia_render::ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }],
    };
    renderer.render_offscreen(device, &scene).expect("frame").pixels
}

fn center(pixels: &[u8]) -> [u8; 4] {
    let i = (32 * 64 + 32) * 4;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

/// The pass is off by default, converts inside the frame once enabled, never touches the frame's
/// own image, and keeps at most one pending picture per player.
#[test]
fn the_frame_pass_is_opt_in_and_leaves_the_frame_unchanged() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let material = LabMaterial {
        base_color: [0.6, 0.6, 0.6, 1.0],
        unlit: true,
        ..LabMaterial::default()
    };
    let mut renderer = FrameRenderer::new();
    assert!(
        !renderer
            .registered_passes()
            .iter()
            .any(|n| n == VIDEO_CONVERT_PASS_NAME),
        "no video pass by default"
    );
    let before = render(&mut renderer, &device, &material);

    let jobs = VideoConvertJobs::default();
    assert!(renderer.enable_video_convert(&jobs));
    assert!(!renderer.enable_video_convert(&jobs), "registered once");
    let old = picture(32, 18, 8, (1, 1), YuvMatrix::Bt709, false, 10);
    let new = picture(32, 18, 8, (1, 1), YuvMatrix::Bt709, false, 11);
    jobs.submit(7, old);
    jobs.submit(7, new.clone());
    assert_eq!(jobs.stats().pending, 1, "one pending picture per player");
    assert_eq!(jobs.stats().replaced, 1);
    let after = render(&mut renderer, &device, &material);
    assert_eq!(before, after, "the convert pass does not touch the frame's image");
    let stats = jobs.stats();
    assert_eq!((stats.converted, stats.pending, stats.failed), (1, 0, 0));
    assert_eq!(stats.gpu_bytes, 2 * 32 * 18 * 4 + (32 * 18 + 2 * 16 * 9) as u64);
    let texture = jobs.texture(7).expect("converted texture");
    let mut cpu = Vec::new();
    assert!(yuv_to_rgba8(&new.planes(), &mut cpu));
    assert!(
        max_diff(&read_rgba(&device, &texture), &cpu) <= 1,
        "the newest picture was converted"
    );
    assert!(jobs.view(7).is_some_and(|(serial, _)| serial == 1));

    // No pending picture: the next frame converts nothing.
    let _ = render(&mut renderer, &device, &material);
    assert_eq!(jobs.stats().converted, 1);
    jobs.remove(7);
    assert!(jobs.texture(7).is_none());
    assert_eq!(jobs.stats().gpu_bytes, 0);
    assert!(renderer.disable_video_convert());
}

/// A material's `video` parameter shows the latest frame written to the player's texture.
#[test]
fn a_video_material_shows_the_latest_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let texture = DynamicTexture::new(8, 8, true);
    let solid = |rgb: [u8; 3]| -> Vec<u8> { (0..64).flat_map(|_| [rgb[0], rgb[1], rgb[2], 255]).collect() };
    texture.write(&solid([230, 20, 20])).expect("red frame");
    let base = LabMaterial {
        base_color: [1.0, 1.0, 1.0, 1.0],
        unlit: true,
        ..LabMaterial::default()
    };
    let material = video_material(&base, &texture, VideoSlot::Albedo);
    let mut renderer = FrameRenderer::new();
    let red = center(&render(&mut renderer, &device, &material));
    assert!(red[0] > 150 && red[0] > red[2] + 80, "red frame shows: {red:?}");
    texture.write(&solid([20, 20, 230])).expect("blue frame");
    let blue = center(&render(&mut renderer, &device, &material));
    assert!(
        blue[2] > 150 && blue[2] > blue[0] + 80,
        "the next frame shows: {blue:?}"
    );
}

/// Compile check that a converted plane view helps no one by accident: the planes type is `Copy`.
#[test]
fn planes_are_cheap_views() {
    let y = [16u8; 4];
    let c = [128u8; 1];
    let p = YuvPlanes {
        width: 2,
        height: 2,
        bit_depth: 8,
        chroma_shift: (1, 1),
        matrix: YuvMatrix::Bt709,
        full_range: false,
        planes: [&y, &c, &c],
    };
    let q = p;
    assert!(p.is_complete() && q.is_complete());
}

fn render_sized(renderer: &mut FrameRenderer, device: &RenderDevice, w: u32, h: u32) -> Vec<u8> {
    let vertices = quad();
    let indices = [0u32, 2, 1, 0, 3, 2];
    let material = LabMaterial {
        base_color: [0.5, 0.5, 0.5, 1.0],
        unlit: true,
        ..LabMaterial::default()
    };
    let scene = FrameScene {
        width: w,
        height: h,
        eye: [0.0, 3.0, 0.01],
        target: [0.0, 0.0, 0.0],
        fov_y: 1.1,
        near: 0.05,
        far: 50.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: None,
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0, 1.0, 1.0], 0.0),
        local_lights: &[],
        quality: FrameQuality::LAB,
        meshes: vec![FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: 1,
            object_key: 1,
            vertices: &vertices,
            indices: &indices,
            material: &material,
            model: Mat4::IDENTITY.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: regia_render::ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }],
    };
    renderer.render_offscreen(device, &scene).expect("frame").pixels
}

fn px(pixels: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * w + x) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

/// A short fullscreen picture must not abort the pass: the well-formed pictures upload and draw
/// on every frame, and only an all-short queue is an error.
#[test]
fn a_short_picture_does_not_drop_the_other_videos() {
    use regia_render::{FullscreenFit, FullscreenVideo, VIDEO_FULLSCREEN_PASS_NAME, VideoFullscreenQueue};
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let (w, h) = (64u32, 48u32);
    let mut renderer = FrameRenderer::new();
    let queue = VideoFullscreenQueue::default();
    assert!(renderer.enable_video_fullscreen(&queue));
    assert!(
        renderer
            .registered_passes()
            .iter()
            .any(|n| n == VIDEO_FULLSCREEN_PASS_NAME)
    );
    let full = FullscreenVideo {
        player: 1,
        layer: 0,
        fit: FullscreenFit::Stretch,
        aspect: 1.0,
        serial: 1,
        width: 2,
        height: 2,
        rgba: Arc::from(vec![
            200u8, 30, 30, 255, 200, 30, 30, 255, 200, 30, 30, 255, 200, 30, 30, 255,
        ]),
    };
    let short = FullscreenVideo {
        player: 2,
        layer: 1,
        fit: FullscreenFit::Stretch,
        aspect: 1.0,
        serial: 1,
        width: 2,
        height: 2,
        rgba: Arc::from(vec![0u8; 4]),
    };
    queue.set(vec![full, short]);
    // Both frames render: the short picture is skipped, the pass is not stuck in Err.
    for round in 0..2 {
        let frame = render_sized(&mut renderer, &device, w, h);
        let p = px(&frame, w, 32, 24);
        assert!(
            p[0] > 150 && p[1] < 80 && p[2] < 80,
            "frame {round}: the full picture draws: {p:?}"
        );
    }
    let stats = queue.stats();
    assert_eq!(stats.drawn, 2, "one draw per frame, the short one skipped");
    assert_eq!(stats.uploads, 1, "the short picture never uploads");
    // Replacing the queue with well-formed pictures draws both.
    queue.set(vec![FullscreenVideo {
        serial: 2,
        rgba: Arc::from([30u8, 30, 200, 255].repeat(4)),
        ..queue.videos()[0].clone()
    }]);
    let frame = render_sized(&mut renderer, &device, w, h);
    let p = px(&frame, w, 32, 24);
    assert!(p[2] > 150, "the replacement draws: {p:?}");
    assert!(renderer.disable_video_fullscreen());
}

/// specs/50 § 2.7 fullscreen: a 2:1 picture (left half red, right half blue) letterboxed into a
/// 4:3 view has black bars above and below and both halves in place; an empty queue leaves the
/// frame byte-identical; a new serial uploads once.
#[test]
fn the_fullscreen_pass_letterboxes_and_is_inert_when_empty() {
    use regia_render::{FullscreenFit, FullscreenVideo, VIDEO_FULLSCREEN_PASS_NAME, VideoFullscreenQueue};
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let (w, h) = (64u32, 48u32);
    let mut renderer = FrameRenderer::new();
    let before = render_sized(&mut renderer, &device, w, h);
    let queue = VideoFullscreenQueue::default();
    assert!(renderer.enable_video_fullscreen(&queue));
    assert!(
        renderer
            .registered_passes()
            .iter()
            .any(|n| n == VIDEO_FULLSCREEN_PASS_NAME)
    );
    assert_eq!(
        render_sized(&mut renderer, &device, w, h),
        before,
        "an empty queue draws nothing"
    );

    let (pw, ph) = (8u32, 4u32);
    let picture: Vec<u8> = (0..pw * ph)
        .flat_map(|i| {
            if i % pw < pw / 2 {
                [255, 0, 0, 255]
            } else {
                [0, 0, 255, 255]
            }
        })
        .collect();
    let video = |fit, serial| FullscreenVideo {
        player: 9,
        layer: 0,
        fit,
        aspect: 2.0,
        serial,
        width: pw,
        height: ph,
        rgba: Arc::from(picture.clone()),
    };
    queue.set(vec![video(FullscreenFit::Letterbox, 1)]);
    let frame = render_sized(&mut renderer, &device, w, h);
    // 2:1 into 64x48: 64x32 at y = 8.
    for (x, y) in [(2, 2), (32, 4), (60, 44)] {
        assert_eq!(px(&frame, w, x, y), [0, 0, 0, 255], "bar at ({x}, {y})");
    }
    let red = px(&frame, w, 8, 24);
    let blue = px(&frame, w, 56, 24);
    assert!(
        red[0] > 240 && red[1] < 10 && red[2] < 10,
        "left half red: {red:?}"
    );
    assert!(
        blue[2] > 240 && blue[0] < 10 && blue[1] < 10,
        "right half blue: {blue:?}"
    );
    assert_eq!(queue.stats().uploads, 1);
    let _ = render_sized(&mut renderer, &device, w, h);
    assert_eq!(queue.stats().uploads, 1, "same serial: no upload");
    assert_eq!(queue.stats().drawn, 2);

    // Stretch covers the whole view: no bar.
    queue.set(vec![video(FullscreenFit::Stretch, 2)]);
    let frame = render_sized(&mut renderer, &device, w, h);
    let top_left = px(&frame, w, 2, 2);
    assert!(top_left[0] > 240, "stretched picture at the corner: {top_left:?}");
    assert_eq!(queue.stats().uploads, 2);

    queue.set(Vec::new());
    assert_eq!(
        render_sized(&mut renderer, &device, w, h),
        before,
        "emptied queue: frame as before"
    );
    assert!(renderer.disable_video_fullscreen());
}
