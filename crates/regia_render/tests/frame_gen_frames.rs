// SPDX-License-Identifier: MPL-2.0
//! Frame generation with real renderer frames (ADR-2067): a camera pans and a box moves through a
//! scene, the interpolator gets the frame's own depth and motion through the temporal seam, and the
//! generated frame is compared with the frame the renderer draws at the true midpoint.

use regia_core::glam::Vec3;
use regia_render::frame_gen::TransferFunction;
use regia_render::frame_gen::passes::{FrameGenFrame, FrameGenPasses};
use regia_render::frame_gen::present::{BlitPipelines, BlitSource, SceneImages};
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, FrameTarget, LabMaterial, LabMesh, LabScene,
    LabVertex, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, TemporalUpscaler,
};

const W: u32 = 192;
const H: u32 = 108;
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> (std::sync::MutexGuard<'static, ()>, RenderDevice) {
    let guard = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match RenderDevice::headless() {
        Ok(device) => (guard, device),
        Err(err) => panic!("no GPU: {err}"),
    }
}

fn quad(corners: [[f32; 3]; 4], normal: [f32; 3], material: usize) -> LabMesh {
    LabMesh {
        vertices: corners
            .iter()
            .zip([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(*p, normal, uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    }
}

/// A floor, a wall, and a row of bright panels against it, so the pan has edges to follow.
fn scene(eye: [f32; 3]) -> LabScene {
    let material = |base_color: [f32; 4]| LabMaterial {
        base_color,
        roughness: 0.7,
        ..LabMaterial::default()
    };
    let mut meshes = vec![
        quad(
            [
                [-80.0, 0.0, 10.0],
                [80.0, 0.0, 10.0],
                [80.0, 0.0, -80.0],
                [-80.0, 0.0, -80.0],
            ],
            [0.0, 1.0, 0.0],
            0,
        ),
        quad(
            [
                [-80.0, 0.0, -20.0],
                [80.0, 0.0, -20.0],
                [80.0, 30.0, -20.0],
                [-80.0, 30.0, -20.0],
            ],
            [0.0, 0.0, 1.0],
            1,
        ),
    ];
    for i in -8i32..=8 {
        let x = i as f32 * 3.0;
        let m = 2 + i.rem_euclid(3) as usize;
        meshes.push(quad(
            [
                [x, 1.0, -19.5],
                [x + 1.5, 1.0, -19.5],
                [x + 1.5, 4.0, -19.5],
                [x, 4.0, -19.5],
            ],
            [0.0, 0.0, 1.0],
            m,
        ));
    }
    LabScene {
        width: W,
        height: H,
        eye,
        target: [eye[0], 3.0, -60.0],
        fov_y: 0.9,
        near: 0.1,
        far: 500.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 30_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.3, -0.6, -0.4], [1.0, 0.95, 0.9], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![
            material([0.3, 0.3, 0.3, 1.0]),
            material([0.5, 0.45, 0.4, 1.0]),
            material([0.9, 0.1, 0.1, 1.0]),
            material([0.1, 0.8, 0.2, 1.0]),
            material([0.1, 0.2, 0.9, 1.0]),
        ],
        meshes,
    }
}

fn post() -> PostSettings {
    PostSettings {
        auto_exposure: false,
        exposure_ev: 14.0,
        bloom: false,
        ..PostSettings::golden_deterministic()
    }
}

fn quality(taa: bool) -> FrameQuality {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = taa;
    quality
}

/// One texture's RGB as bytes.
fn read_rgba8(device: &RenderDevice, texture: &wgpu::Texture) -> Vec<u8> {
    let row = (W * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let staging = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("test readback"),
        size: u64::from(row * H),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(H),
            },
        },
        texture.size(),
    );
    device.queue.submit([encoder.finish()]);
    let slice = staging.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.unwrap_or_else(|e| panic!("{e}")));
    device
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .unwrap_or_else(|e| panic!("{e}"));
    let bytes = slice.get_mapped_range().unwrap_or_else(|e| panic!("{e:?}"));
    let mut out = Vec::with_capacity((W * H * 4) as usize);
    for y in 0..H {
        let start = (y * row) as usize;
        out.extend_from_slice(&bytes[start..start + (W * 4) as usize]);
    }
    out
}

/// Mean absolute difference per channel, 0–255.
fn mae(a: &[u8], b: &[u8]) -> f32 {
    assert_eq!(a.len(), b.len());
    let sum: u64 = a
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .map(|(x, y)| (0..3).map(|c| u64::from(x[c].abs_diff(y[c]))).sum::<u64>())
        .sum();
    sum as f32 / (3 * W * H) as f32
}

/// Renders into the scene image in progress, then advances it.
fn render_scene(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    images: &SceneImages,
    eye: [f32; 3],
    taa: bool,
) {
    let lab = scene(eye);
    let post = post();
    let mut frame = lab.as_frame_scene_with_quality(quality(taa));
    frame.post = Some(&post);
    let image = images.current();
    let target = FrameTarget::full(&image.view, FORMAT, W, H).with_color(Some(&image.texture));
    renderer
        .render_into(device, &frame, target)
        .unwrap_or_else(|e| panic!("{e}"));
}

/// The generated frame between `from` and `to`, the true midpoint frame, and both real frames.
fn generate(
    device: &RenderDevice,
    from: [f32; 3],
    to: [f32; 3],
    taa: bool,
    upscaler: TemporalUpscaler,
) -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut renderer = FrameRenderer::new();
    renderer.set_temporal_upscaler(upscaler);
    renderer.set_frame_gen_capture(true);
    let mut images = SceneImages::new(&device.device, (W, H), FORMAT);
    let mut passes = FrameGenPasses::default();
    let mut blit = BlitPipelines::default();
    let display = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test display"),
        size: wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let display_view = display.create_view(&wgpu::TextureViewDescriptor::default());
    // Settle the temporal state on the first position, feeding the interpolator's history.
    let mut previous_rgba = Vec::new();
    let mut current_rgba = Vec::new();
    let steps = [from, from, from, to];
    for (i, eye) in steps.iter().enumerate() {
        render_scene(device, &mut renderer, &images, *eye, taa);
        let inputs = renderer
            .frame_gen_inputs()
            .unwrap_or_else(|| panic!("the seam hands the inputs over"))
            .clone();
        assert_eq!(inputs.render_size, (W, H));
        assert_eq!(inputs.velocity.is_some(), taa);
        let last = i == steps.len() - 1;
        let input = FrameGenFrame {
            current_color: &images.current().raw_view,
            previous_color: &images.previous().raw_view,
            depth: &inputs.depth,
            velocity: inputs.velocity.as_ref(),
            reactive: inputs.reactive.as_ref(),
            render_size: inputs.render_size,
            display_size: (W, H),
            view_proj: inputs.view_proj,
            unjittered_view_proj: inputs.unjittered_view_proj,
            velocity_prev_view_proj: inputs.velocity_prev_view_proj,
            eye: inputs.eye,
            fov_y: inputs.fov_y,
            transfer: TransferFunction::Srgb,
            min_max_luminance: [0.0, 80.0],
            reset: i == 0,
            debug_tear_lines: false,
            debug_view: false,
        };
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        passes
            .record(&device.device, &device.queue, &mut encoder, &input, last)
            .unwrap_or_else(|e| panic!("{e}"));
        device.queue.submit([encoder.finish()]);
        if last {
            previous_rgba = read_rgba8(device, &images.previous().texture);
            current_rgba = read_rgba8(device, &images.current().texture);
        }
        images.advance();
    }
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    let output = passes.output_view().unwrap_or_else(|| panic!("no output"));
    blit.record(
        &device.device,
        &mut encoder,
        output,
        BlitSource::Interpolated,
        &display_view,
        FORMAT,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    device.queue.submit([encoder.finish()]);
    let generated = read_rgba8(device, &display);

    // The true midpoint from a fresh renderer on the same settle pattern.
    let mid = (Vec3::from(from) + Vec3::from(to)) * 0.5;
    let mut truth = FrameRenderer::new();
    truth.set_temporal_upscaler(upscaler);
    let truth_images = SceneImages::new(&device.device, (W, H), FORMAT);
    for _ in 0..4 {
        render_scene(device, &mut truth, &truth_images, mid.to_array(), taa);
    }
    let midpoint = read_rgba8(device, &truth_images.current().texture);
    (generated, midpoint, previous_rgba, current_rgba)
}

fn assert_closer_to_the_midpoint(what: &str, frames: &(Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>)) {
    let (generated, midpoint, previous, current) = frames;
    let to_truth = mae(generated, midpoint);
    let prev_truth = mae(previous, midpoint);
    let cur_truth = mae(current, midpoint);
    eprintln!(
        "{what}: generated {to_truth:.3}, previous {prev_truth:.3}, current {cur_truth:.3} (mean abs diff to the true midpoint)"
    );
    assert!(
        prev_truth > 1.0 && cur_truth > 1.0,
        "the pan must move the image: {prev_truth}, {cur_truth}"
    );
    assert!(
        to_truth < 0.5 * prev_truth.min(cur_truth),
        "{what}: the generated frame ({to_truth}) must be well closer to the true midpoint than either real frame ({prev_truth}, {cur_truth})"
    );
}

#[test]
fn a_panning_camera_interpolates_toward_the_true_midpoint_without_taa() {
    let (_gpu, device) = device();
    let frames = generate(
        &device,
        [-1.0, 3.0, 0.0],
        [1.0, 3.0, 0.0],
        false,
        TemporalUpscaler::Taau,
    );
    assert_closer_to_the_midpoint("no taa", &frames);
}

#[test]
fn a_panning_camera_interpolates_behind_each_upscaler() {
    let (_gpu, device) = device();
    for upscaler in [
        TemporalUpscaler::Taau,
        TemporalUpscaler::Tsr,
        TemporalUpscaler::Fsr3,
    ] {
        let frames = generate(&device, [-1.0, 3.0, 0.0], [1.0, 3.0, 0.0], true, upscaler);
        assert_closer_to_the_midpoint(&format!("{upscaler:?}"), &frames);
    }
}

#[test]
fn without_the_capture_the_renderer_keeps_nothing() {
    let (_gpu, device) = device();
    let mut renderer = FrameRenderer::new();
    let images = SceneImages::new(&device.device, (W, H), FORMAT);
    render_scene(&device, &mut renderer, &images, [0.0, 3.0, 0.0], false);
    assert!(renderer.frame_gen_inputs().is_none());
    renderer.set_frame_gen_capture(true);
    render_scene(&device, &mut renderer, &images, [0.0, 3.0, 0.0], false);
    assert!(renderer.frame_gen_inputs().is_some());
    renderer.set_frame_gen_capture(false);
    assert!(renderer.frame_gen_inputs().is_none());
}

#[test]
fn the_rendered_frame_reaches_the_target_bit_for_bit() {
    let (_gpu, device) = device();
    let mut renderer = FrameRenderer::new();
    let images = SceneImages::new(&device.device, (W, H), FORMAT);
    render_scene(&device, &mut renderer, &images, [0.0, 3.0, 0.0], false);
    let rendered = read_rgba8(&device, &images.current().texture);
    let display = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test display"),
        size: images.current().texture.size(),
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = display.create_view(&wgpu::TextureViewDescriptor::default());
    let mut blit = BlitPipelines::default();
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    blit.record(
        &device.device,
        &mut encoder,
        &images.current().view,
        BlitSource::SameFormat,
        &view,
        FORMAT,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    device.queue.submit([encoder.finish()]);
    assert_eq!(read_rgba8(&device, &display), rendered);
}
