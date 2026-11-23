// SPDX-License-Identifier: MPL-2.0
//! The frame generation passes on a real device (ADR-2067): the WGSL optical flow equals its CPU
//! reference vector for vector, the scene-change test agrees with its reference, the interpolator
//! puts a moving square where the CPU reference puts it, a reset copies the frame, and steady frames
//! create nothing.

use regia_core::glam::camera::rh as camera_rh;
use regia_core::glam::{Mat4, Vec3, Vec4};
use regia_render::frame_gen::TransferFunction;
use regia_render::frame_gen::optical_flow::{self, FlowField, LumaImage, SceneChangeDetector, estimate_flow};
use regia_render::frame_gen::passes::{FrameGenFrame, FrameGenPasses};
use regia_render::frame_gen::reference::{ColorImage, InterpolationInputs, interpolate};
use regia_render::{FramePathKind, RenderDevice, take_frame_path_events, trace_frame_path};

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

/// The deterministic texture of `optical_flow.rs`'s tests: detail at every pyramid level.
fn noise(x: i32, y: i32) -> u8 {
    let hash = |cx: i32, cy: i32, salt: u32| -> u32 {
        let mut h = (cx as u32).wrapping_mul(0x9E37_79B1) ^ (cy as u32).wrapping_mul(0x85EB_CA77) ^ salt;
        h ^= h >> 15;
        h = h.wrapping_mul(0x2C1B_3C6D);
        h ^= h >> 12;
        h & 0xFF
    };
    let mut sum = 0u32;
    for (i, cell) in [1, 2, 4, 8, 16].into_iter().enumerate() {
        sum += hash(x.div_euclid(cell), y.div_euclid(cell), i as u32);
    }
    (sum / 5) as u8
}

fn shifted(size: (u32, u32), shift: (i32, i32)) -> LumaImage {
    LumaImage::from_fn(size.0, size.1, |x, y| {
        noise(x as i32 - shift.0, y as i32 - shift.1)
    })
}

fn color_texture(device: &RenderDevice, size: (u32, u32), rgba: &[u8]) -> wgpu::TextureView {
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test color"),
        size: wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    device.queue.write_texture(
        texture.as_image_copy(),
        rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(size.0 * 4),
            rows_per_image: Some(size.1),
        },
        texture.size(),
    );
    texture.create_view(&wgpu::TextureViewDescriptor::default())
}

fn luma_texture(device: &RenderDevice, image: &LumaImage) -> wgpu::TextureView {
    let rgba: Vec<u8> = image.pixels().iter().flat_map(|&g| [g, g, g, 255]).collect();
    color_texture(device, image.size(), &rgba)
}

/// A depth target whose rectangles hold fixed device depths, later ones on top.
fn depth_texture(device: &RenderDevice, size: (u32, u32), rects: &[([u32; 4], f32)]) -> wgpu::TextureView {
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test depth"),
        size: wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Depth32Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let module = device.device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("test depth"),
        source: wgpu::ShaderSource::Wgsl(
            "@vertex fn vs_main(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
                let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
                return vec4<f32>(uv * 2.0 - vec2<f32>(1.0), 0.0, 1.0);
            }"
            .into(),
        ),
    });
    let pipeline = device
        .device
        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("test depth"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: None,
            multiview_mask: None,
            cache: None,
        });
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("test depth"),
            color_attachments: &[],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            ..Default::default()
        });
        pass.set_pipeline(&pipeline);
        for ([x, y, w, h], depth) in rects {
            pass.set_viewport(*x as f32, *y as f32, *w as f32, *h as f32, *depth, *depth);
            pass.draw(0..3, 0..1);
        }
    }
    device.queue.submit([encoder.finish()]);
    view
}

fn velocity_texture(device: &RenderDevice, size: (u32, u32), texels: &[[f32; 4]]) -> wgpu::TextureView {
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test velocity"),
        size: wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba32Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    device.queue.write_texture(
        texture.as_image_copy(),
        bytemuck::cast_slice(texels),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(size.0 * 16),
            rows_per_image: Some(size.1),
        },
        texture.size(),
    );
    texture.create_view(&wgpu::TextureViewDescriptor::default())
}

fn camera() -> (Mat4, [f32; 3]) {
    let eye = Vec3::ZERO;
    let view = camera_rh::view::look_at_mat4(eye, Vec3::new(0.0, 0.0, -1.0), Vec3::Y);
    (
        camera_rh::proj::directx::perspective(1.0, 4.0 / 3.0, 0.1, 1000.0) * view,
        eye.to_array(),
    )
}

/// [`camera`] with no far plane: the sky (device depth 1, the clear value) unprojects to a point
/// at infinity, which the host may hand frame generation directly (`DepthConvention::infinite`).
fn infinite_camera() -> (Mat4, [f32; 3]) {
    let eye = Vec3::ZERO;
    let view = camera_rh::view::look_at_mat4(eye, Vec3::new(0.0, 0.0, -1.0), Vec3::Y);
    (
        camera_rh::proj::directx::perspective_infinite(1.0, 4.0 / 3.0, 0.1) * view,
        eye.to_array(),
    )
}

/// Device depth of a surface `view_z` metres in front of [`camera`].
fn device_depth(view_z: f32) -> f32 {
    device_depth_of(camera(), view_z)
}

/// Device depth of a surface `view_z` metres in front of `camera`.
fn device_depth_of(camera: (Mat4, [f32; 3]), view_z: f32) -> f32 {
    let clip = camera.0 * Vec4::new(0.0, 0.0, -view_z, 1.0);
    clip.z / clip.w
}

struct Inputs<'a> {
    current: &'a wgpu::TextureView,
    previous: &'a wgpu::TextureView,
    depth: &'a wgpu::TextureView,
    velocity: Option<&'a wgpu::TextureView>,
    size: (u32, u32),
    reset: bool,
    debug: (bool, bool),
}

/// The reactive mask of [`run_masked`].
type Mask<'a> = Option<&'a wgpu::TextureView>;

fn run(device: &RenderDevice, passes: &mut FrameGenPasses, inputs: &Inputs<'_>, generate: bool) {
    run_masked(device, passes, inputs, None, generate);
}

fn run_masked(
    device: &RenderDevice,
    passes: &mut FrameGenPasses,
    inputs: &Inputs<'_>,
    reactive: Mask<'_>,
    generate: bool,
) {
    run_with_camera(device, passes, inputs, reactive, camera(), generate);
}

fn run_with_camera(
    device: &RenderDevice,
    passes: &mut FrameGenPasses,
    inputs: &Inputs<'_>,
    reactive: Mask<'_>,
    camera: (Mat4, [f32; 3]),
    generate: bool,
) {
    let (vp, eye) = camera;
    let frame = FrameGenFrame {
        current_color: inputs.current,
        previous_color: inputs.previous,
        depth: inputs.depth,
        velocity: inputs.velocity,
        reactive,
        render_size: inputs.size,
        display_size: inputs.size,
        view_proj: vp,
        unjittered_view_proj: vp,
        velocity_prev_view_proj: vp,
        eye,
        fov_y: 1.0,
        transfer: TransferFunction::Srgb,
        min_max_luminance: [0.0, 80.0],
        reset: inputs.reset,
        debug_tear_lines: inputs.debug.0,
        debug_view: inputs.debug.1,
    };
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    passes
        .record(&device.device, &device.queue, &mut encoder, &frame, generate)
        .unwrap_or_else(|e| panic!("record: {e}"));
    device.queue.submit([encoder.finish()]);
}

fn read_buffer(device: &RenderDevice, source: &wgpu::Buffer) -> Vec<u8> {
    let staging = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("test readback"),
        size: source.size(),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.copy_buffer_to_buffer(source, 0, &staging, 0, source.size());
    device.queue.submit([encoder.finish()]);
    let slice = staging.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.unwrap_or_else(|e| panic!("{e}")));
    device
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .unwrap_or_else(|e| panic!("{e}"));
    slice
        .get_mapped_range()
        .unwrap_or_else(|e| panic!("{e:?}"))
        .to_vec()
}

fn flow_field(device: &RenderDevice, passes: &FrameGenPasses) -> FlowField {
    let (buffer, blocks) = passes.flow_field().unwrap_or_else(|| panic!("no field"));
    let bytes = read_buffer(device, buffer);
    let words: &[i32] = bytemuck::cast_slice(&bytes);
    let vectors = words
        .as_chunks::<2>()
        .0
        .iter()
        .take(blocks.0 as usize * blocks.1 as usize)
        .map(|v| [v[0] as i16, v[1] as i16])
        .collect();
    FlowField {
        blocks,
        block_size: optical_flow::BLOCK_SIZE,
        vectors,
    }
}

/// `[scene history bits, frames since reset]`.
fn state(device: &RenderDevice, passes: &FrameGenPasses) -> [u32; 2] {
    let buffer = passes.state_buffer().unwrap_or_else(|| panic!("no state"));
    let bytes = read_buffer(device, buffer);
    let words: &[u32] = bytemuck::cast_slice(&bytes);
    [words[0], words[1]]
}

/// An IEEE binary16 value (the output's format).
fn f32_from_half(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exponent = i32::from((bits >> 10) & 0x1f);
    let fraction = f32::from(bits & 0x3ff) / 1024.0;
    match exponent {
        0 => sign * fraction * (-14f32).exp2(),
        31 if fraction == 0.0 => sign * f32::INFINITY,
        31 => f32::NAN,
        e => sign * (1.0 + fraction) * ((e - 15) as f32).exp2(),
    }
}

/// The output image as RGBA f32 rows.
fn output_pixels(device: &RenderDevice, passes: &FrameGenPasses, size: (u32, u32)) -> Vec<[f32; 4]> {
    let texture = passes.output_texture().unwrap_or_else(|| panic!("no output"));
    let row = (size.0 * 8).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let staging = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("test output"),
        size: u64::from(row * size.1),
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
                rows_per_image: Some(size.1),
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
    let mut out = Vec::with_capacity((size.0 * size.1) as usize);
    for y in 0..size.1 {
        let start = (y * row) as usize;
        let line: &[u16] = bytemuck::cast_slice(&bytes[start..start + (size.0 * 8) as usize]);
        for p in line.as_chunks::<4>().0 {
            out.push(p.map(f32_from_half));
        }
    }
    out
}

/// The dilated motion target (`Rgba32Float`) as RGBA rows.
fn dilated_motion(device: &RenderDevice, passes: &FrameGenPasses, size: (u32, u32)) -> Vec<[f32; 4]> {
    let texture = passes
        .dilated_motion_texture()
        .unwrap_or_else(|| panic!("no motion"));
    let row = (size.0 * 16).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let staging = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("test motion"),
        size: u64::from(row * size.1),
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
                rows_per_image: Some(size.1),
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
    let mut out = Vec::with_capacity((size.0 * size.1) as usize);
    for y in 0..size.1 {
        let start = (y * row) as usize;
        let line: &[f32] = bytemuck::cast_slice(&bytes[start..start + (size.0 * 16) as usize]);
        out.extend(line.as_chunks::<4>().0.iter().copied());
    }
    out
}

/// The sky of an infinite projection (device depth 1) unprojects to a point at infinity,
/// `w = 0`: its camera motion is the camera's rotation only (zero for a still camera), never the
/// NaN of dividing by `w` (audit-v035-wgsl 3). Every later pass reads this target.
#[test]
fn an_infinite_projections_sky_has_finite_camera_motion() {
    let (_gpu, device) = device();
    let size = (W, H);
    let camera = infinite_camera();
    let color = color_texture(&device, size, &square_rgba(16));
    let depth = depth_texture(
        &device,
        size,
        &[([16, 20, SQUARE, SQUARE], device_depth_of(camera, NEAR))],
    );
    let mut passes = FrameGenPasses::default();
    for reset in [true, false] {
        run_with_camera(
            &device,
            &mut passes,
            &Inputs {
                current: &color,
                previous: &color,
                depth: &depth,
                velocity: None,
                size,
                reset,
                debug: (false, false),
            },
            None,
            camera,
            !reset,
        );
    }
    let motion = dilated_motion(&device, &passes, size);
    let sky = motion[(H as usize - 1) * W as usize];
    assert_eq!(sky[2], 1.0, "a sky texel: {sky:?}");
    // NaN fails both comparisons.
    let still = |m: &[f32; 4]| m[0].abs() < 1.0e-6 && m[1].abs() < 1.0e-6;
    let bad: Vec<(usize, [f32; 4])> = motion
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, m)| !still(m))
        .collect();
    assert!(
        bad.is_empty(),
        "{} texels of a still camera move or are not finite, first {:?}",
        bad.len(),
        &bad[..bad.len().min(4)]
    );
}

/// The field for `previous` then `current`, through the GPU passes.
fn gpu_flow(device: &RenderDevice, previous: &LumaImage, current: &LumaImage) -> FlowField {
    let size = current.size();
    let prev_view = luma_texture(device, previous);
    let cur_view = luma_texture(device, current);
    let depth = depth_texture(device, size, &[]);
    let mut passes = FrameGenPasses::default();
    run(
        device,
        &mut passes,
        &Inputs {
            current: &prev_view,
            previous: &prev_view,
            depth: &depth,
            velocity: None,
            size,
            reset: true,
            debug: (false, false),
        },
        false,
    );
    run(
        device,
        &mut passes,
        &Inputs {
            current: &cur_view,
            previous: &prev_view,
            depth: &depth,
            velocity: None,
            size,
            reset: false,
            debug: (false, false),
        },
        true,
    );
    flow_field(device, &passes)
}

#[test]
fn the_gpu_flow_equals_the_cpu_reference_vector_for_vector() {
    let (_gpu, device) = device();
    for (size, shift) in [
        ((128, 128), (13, -6)),
        ((96, 64), (5, 3)),
        ((200, 120), (-21, 9)),
        // Odd sizes: blocks cut by the edge, levels rounded up.
        ((101, 77), (3, 2)),
        ((64, 64), (0, 0)),
    ] {
        let previous = shifted(size, (0, 0));
        let current = shifted(size, shift);
        let expected = estimate_flow(&previous, &current, optical_flow::MAX_LEVELS)
            .unwrap_or_else(|| panic!("sizes match"));
        let actual = gpu_flow(&device, &previous, &current);
        assert_eq!(actual.blocks, expected.blocks, "{size:?}");
        assert_eq!(actual.vectors, expected.vectors, "{size:?} shift {shift:?}");
    }
}

/// The pyramid level the GPU's scene-change test reads: the third, or the coarsest when shorter.
fn scene_level(image: &LumaImage) -> LumaImage {
    image.downsample().downsample()
}

#[test]
fn the_scene_change_test_agrees_with_its_reference() {
    let (_gpu, device) = device();
    let size = (96, 96);
    let a = shifted(size, (0, 0));
    let pan = shifted(size, (5, 3));
    let cut = LumaImage::from_fn(96, 96, |x, y| 255 - noise(x as i32, y as i32) / 4);
    let mut reference = SceneChangeDetector::default();
    assert!(!reference.update(&scene_level(&a)));
    assert!(!reference.update(&scene_level(&pan)));
    assert!(reference.update(&scene_level(&cut)));

    let depth = depth_texture(&device, size, &[]);
    let (a_view, pan_view, cut_view) = (
        luma_texture(&device, &a),
        luma_texture(&device, &pan),
        luma_texture(&device, &cut),
    );
    let mut passes = FrameGenPasses::default();
    let frame = |current, previous, reset| Inputs {
        current,
        previous,
        depth: &depth,
        velocity: None,
        size,
        reset,
        debug: (false, false),
    };
    run(&device, &mut passes, &frame(&a_view, &a_view, true), false);
    assert_eq!(
        state(&device, &passes),
        [0, 0],
        "the first frame starts a history"
    );
    run(&device, &mut passes, &frame(&pan_view, &a_view, false), true);
    assert_eq!(state(&device, &passes), [0, 1], "a pan keeps the scene");
    run(&device, &mut passes, &frame(&cut_view, &pan_view, false), true);
    let [history, frames] = state(&device, &passes);
    assert_eq!(history & 1, 1, "the cut is seen");
    assert_eq!(frames, 0, "the cut restarts the count");
    // A scene change returns no motion, as the SDK's estimator does.
    let field = flow_field(&device, &passes);
    assert!(field.vectors.iter().all(|v| *v == [0, 0]), "{:?}", field.vectors);
    // The reset frame copies the image.
    let out = output_pixels(&device, &passes, size);
    for (i, &g) in cut.pixels().iter().enumerate() {
        assert!((out[i][0] - f32::from(g) / 255.0).abs() < 1.0e-3, "pixel {i}");
    }
}

const W: u32 = 64;
const H: u32 = 48;
const SQUARE: u32 = 8;
const NEAR: f32 = 5.0;
const FAR: f32 = 100.0;
const BACKGROUND: [f32; 3] = [0.2, 0.25, 0.3];
const FOREGROUND: [f32; 3] = [1.0, 0.9, 0.1];

fn in_square(x: u32, y: u32, left: u32) -> bool {
    (left..left + SQUARE).contains(&x) && (20..20 + SQUARE).contains(&y)
}

fn square_rgba(left: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((W * H * 4) as usize);
    for y in 0..H {
        for x in 0..W {
            let c = if in_square(x, y, left) {
                FOREGROUND
            } else {
                BACKGROUND
            };
            out.extend(c.map(|v| (v * 255.0).round() as u8));
            out.push(255);
        }
    }
    out
}

fn square_rects(left: u32) -> [([u32; 4], f32); 2] {
    [
        ([0, 0, W, H], device_depth(FAR)),
        ([left, 20, SQUARE, SQUARE], device_depth(NEAR)),
    ]
}

fn linear_like_stored(c: [f32; 3]) -> [f32; 3] {
    // The 8-bit values the textures hold.
    c.map(|v| (v * 255.0).round() / 255.0)
}

#[test]
fn the_interpolator_puts_a_moving_square_where_the_reference_does() {
    moving_square_matches_the_reference(camera(), false);
}

/// The moving square of [`the_interpolator_puts_a_moving_square_where_the_reference_does`] in
/// front of the sky of an infinite projection: the background is device depth 1, which
/// unprojects to a point at infinity (`w = 0`). Its camera motion must stay finite (zero here,
/// the camera is still) and the sky must not outrank the square in the game-vector field as a
/// "nearest" surface: the generated frame is the same as with a far plane (audit-v035-wgsl 2,
/// 3).
#[test]
fn an_infinite_projections_sky_interpolates_like_a_far_background() {
    moving_square_matches_the_reference(infinite_camera(), true);
}

/// Runs the moving square under `camera`; `sky`: the background is left at the clear depth 1
/// instead of a surface at `FAR`.
fn moving_square_matches_the_reference(camera: (Mat4, [f32; 3]), sky: bool) {
    let (_gpu, device) = device();
    let rects = |left: u32| -> Vec<([u32; 4], f32)> {
        let square = ([left, 20, SQUARE, SQUARE], device_depth_of(camera, NEAR));
        if sky {
            vec![square]
        } else {
            vec![([0, 0, W, H], device_depth_of(camera, FAR)), square]
        }
    };
    let size = (W, H);
    let (left_prev, left_cur) = (16u32, 32u32);
    let prev_color = color_texture(&device, size, &square_rgba(left_prev));
    let cur_color = color_texture(&device, size, &square_rgba(left_cur));
    let prev_depth = depth_texture(&device, size, &rects(left_prev));
    let cur_depth = depth_texture(&device, size, &rects(left_cur));
    // The square moved right: its previous position is 16 px to the left, in UV units.
    let motion = [-((left_cur - left_prev) as f32) / W as f32, 0.0];
    let mut texels = vec![[0.0f32; 4]; (W * H) as usize];
    for y in 0..H {
        for x in 0..W {
            if in_square(x, y, left_cur) {
                texels[(y * W + x) as usize] = [motion[0], motion[1], NEAR, 1.0];
            }
        }
    }
    let velocity = velocity_texture(&device, size, &texels);

    let mut passes = FrameGenPasses::default();
    run_with_camera(
        &device,
        &mut passes,
        &Inputs {
            current: &prev_color,
            previous: &prev_color,
            depth: &prev_depth,
            velocity: None,
            size,
            reset: true,
            debug: (false, false),
        },
        None,
        camera,
        false,
    );
    run_with_camera(
        &device,
        &mut passes,
        &Inputs {
            current: &cur_color,
            previous: &prev_color,
            depth: &cur_depth,
            velocity: Some(&velocity),
            size,
            reset: false,
            debug: (false, false),
        },
        None,
        camera,
        true,
    );
    let out = output_pixels(&device, &passes, size);
    let non_finite: Vec<usize> = (0..out.len())
        .filter(|&i| !out[i].iter().all(|c| c.is_finite()))
        .collect();
    assert!(
        non_finite.is_empty(),
        "{} output pixels are not finite, first {:?}",
        non_finite.len(),
        &non_finite[..non_finite.len().min(8)]
    );

    // The CPU reference at t = 0.5.
    let image = |left| {
        ColorImage::from_fn(W, H, |x, y| {
            linear_like_stored(if in_square(x, y, left) {
                FOREGROUND
            } else {
                BACKGROUND
            })
        })
    };
    let depths = |left| -> Vec<f32> {
        (0..H)
            .flat_map(|y| (0..W).map(move |x| (x, y)))
            .map(|(x, y)| if in_square(x, y, left) { NEAR } else { FAR })
            .collect()
    };
    let motion_px: Vec<[f32; 2]> = (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .map(|(x, y)| {
            if in_square(x, y, left_cur) {
                [-((left_cur - left_prev) as f32), 0.0]
            } else {
                [0.0, 0.0]
            }
        })
        .collect();
    let (previous, current) = (image(left_prev), image(left_cur));
    let (current_depth, previous_depth) = (depths(left_cur), depths(left_prev));
    let expected = interpolate(&InterpolationInputs {
        previous: &previous,
        current: &current,
        motion_px: &motion_px,
        current_depth: &current_depth,
        previous_depth: &previous_depth,
        t: 0.5,
        reset: false,
    })
    .unwrap_or_else(|| panic!("sizes match"));

    let left_mid = (left_prev + left_cur) / 2;
    let mut wrong = Vec::new();
    let mut halo = (0usize, 0.0f32);
    for y in 0..H {
        for x in 0..W {
            let i = (y * W + x) as usize;
            let want = expected.image.pixels[i];
            let got = out[i];
            let diff = (0..3).map(|c| (got[c] - want[c]).abs()).fold(0.0, f32::max);
            if diff <= 2.0 / 255.0 {
                continue;
            }
            // The reference is a nearest-pixel oracle. The SDK's method writes each vector to the
            // whole bilinear footprint and inpaints what neither frame shows from a blurred
            // pyramid, so a faint halo around the moving edges is its look: bounded in place and
            // in strength here: it stays inside the area the moving edge just crossed (half the
            // motion on each side). Anywhere else, the two must agree.
            let half_motion = (left_cur - left_prev) / 2;
            let near_x = x + half_motion >= left_mid && x < left_mid + SQUARE + half_motion;
            let near_y = y + 4 >= 20 && y < 20 + SQUARE + 4;
            if near_x && near_y && diff <= 0.15 {
                halo = (halo.0 + 1, halo.1.max(diff));
            } else {
                wrong.push((x, y, got, want));
            }
        }
    }
    eprintln!("moving square: {} halo pixels, strongest {:.3}", halo.0, halo.1);
    assert!(
        wrong.is_empty(),
        "{} pixels differ: {:?}",
        wrong.len(),
        &wrong[..wrong.len().min(8)]
    );
    // The halo stays a small part of the square's surroundings.
    assert!(halo.0 <= (SQUARE * SQUARE) as usize, "{halo:?}");
    // The square's middle is exactly where it should be, and its old and new places are clean.
    let at = |x: u32, y: u32| out[(y * W + x) as usize];
    assert!((at(left_mid + 4, 24)[0] - FOREGROUND[0]).abs() < 2.0 / 255.0);
    assert!((at(left_prev + 2, 24)[2] - BACKGROUND[2]).abs() < 2.0 / 255.0);
    assert!((at(left_cur + 6, 24)[2] - BACKGROUND[2]).abs() < 2.0 / 255.0);
}

#[test]
fn a_reset_copies_the_current_frame_and_the_debug_views_run() {
    let (_gpu, device) = device();
    let size = (W, H);
    let prev = square_rgba(16);
    let cur = square_rgba(24);
    let prev_color = color_texture(&device, size, &prev);
    let cur_color = color_texture(&device, size, &cur);
    let depth = depth_texture(&device, size, &square_rects(24));
    let scope = device.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let mut passes = FrameGenPasses::default();
    let frame = |reset, debug| Inputs {
        current: &cur_color,
        previous: &prev_color,
        depth: &depth,
        velocity: None,
        size,
        reset,
        debug,
    };
    run(&device, &mut passes, &frame(true, (false, false)), false);
    run(&device, &mut passes, &frame(true, (false, false)), true);
    let out = output_pixels(&device, &passes, size);
    for (i, p) in out.iter().enumerate() {
        for c in 0..3 {
            let want = f32::from(cur[i * 4 + c]) / 255.0;
            assert!(
                (p[c] - want).abs() < 1.0e-3,
                "pixel {i} channel {c}: {} vs {want}",
                p[c]
            );
        }
    }
    // Tear lines: the left bar is green, the right one takes the cycling color.
    run(&device, &mut passes, &frame(false, (true, false)), true);
    let out = output_pixels(&device, &passes, size);
    assert_eq!(out[(10 * W + 4) as usize][1], 1.0);
    // The debug view fills its panes without a validation error.
    run(&device, &mut passes, &frame(false, (false, true)), true);
    let out = output_pixels(&device, &passes, size);
    assert!(out.iter().all(|p| p[3] == 1.0 && p.iter().all(|v| v.is_finite())));
    let error = regia_core::tasks::block_on(scope.pop());
    assert!(error.is_none(), "{error:?}");
}

#[test]
fn steady_frames_create_nothing() {
    let (_gpu, device) = device();
    let size = (W, H);
    let a = color_texture(&device, size, &square_rgba(16));
    let b = color_texture(&device, size, &square_rgba(24));
    let depth = depth_texture(&device, size, &square_rects(24));
    let mut passes = FrameGenPasses::default();
    let frame = |current, previous| Inputs {
        current,
        previous,
        depth: &depth,
        velocity: None,
        size,
        reset: false,
        debug: (false, false),
    };
    // Both parities once: each builds its groups.
    run(&device, &mut passes, &frame(&a, &b), true);
    run(&device, &mut passes, &frame(&b, &a), true);
    trace_frame_path(true);
    let _ = take_frame_path_events();
    for _ in 0..3 {
        run(&device, &mut passes, &frame(&a, &b), true);
        run(&device, &mut passes, &frame(&b, &a), true);
    }
    let events = take_frame_path_events();
    trace_frame_path(false);
    let creates: Vec<_> = events
        .iter()
        .filter(|e| e.kind != FramePathKind::BlockingPoll)
        .collect();
    assert!(creates.is_empty(), "{creates:?}");
    assert_eq!(passes.frames(), 8);
}

#[test]
fn a_reactive_mask_lowers_the_trust_in_the_game_vector_only_where_it_is_set() {
    let (_gpu, device) = device();
    let size = (W, H);
    // The flow sees the noise move; with no velocity target the game vectors say it stood still,
    // so the two disagree everywhere and the mask decides.
    let a = shifted(size, (0, 0));
    let b = shifted(size, (6, 0));
    let (a_view, b_view) = (luma_texture(&device, &a), luma_texture(&device, &b));
    let depth = depth_texture(&device, size, &[]);
    // Red = 1 on the left half.
    let mut mask_rgba = Vec::with_capacity((W * H * 4) as usize);
    for _ in 0..H {
        for x in 0..W {
            mask_rgba.extend_from_slice(&[if x < W / 2 { 255 } else { 0 }, 0, 0, 255]);
        }
    }
    let mask = color_texture(&device, size, &mask_rgba);
    let generated = |reactive: Mask<'_>| {
        let mut passes = FrameGenPasses::default();
        // Past the first frames after a reset, which trust the game vector whatever the images say.
        for i in 0..14 {
            let (current, previous) = if i % 2 == 0 {
                (&a_view, &b_view)
            } else {
                (&b_view, &a_view)
            };
            let frame = Inputs {
                current,
                previous,
                depth: &depth,
                velocity: None,
                size,
                reset: i == 0,
                debug: (false, false),
            };
            run_masked(&device, &mut passes, &frame, reactive, true);
        }
        output_pixels(&device, &passes, size)
    };
    let plain = generated(None);
    let masked = generated(Some(&mask));
    let differs = |x: u32, y: u32| {
        let i = (y * W + x) as usize;
        (0..3).any(|c| (plain[i][c] - masked[i][c]).abs() > 1.0 / 255.0)
    };
    let count = |range: std::ops::Range<u32>| {
        (0..H)
            .flat_map(|y| range.clone().map(move |x| (x, y)))
            .filter(|&(x, y)| differs(x, y))
            .count()
    };
    // One texel of bilinear mask around the boundary is neither side.
    let left = count(0..W / 2 - 1);
    let right = count(W / 2 + 1..W);
    assert!(left > (W * H / 8) as usize, "the mask changes its side: {left}");
    assert_eq!(right, 0, "the side without the mask is untouched");
}

/// The GPU cost of one generated frame, printed (real hardware: `-- --ignored --nocapture`). The
/// noise is the search's worst case: no candidate stops early.
#[test]
#[ignore = "a measurement, not a check; run it on real hardware"]
fn cost_of_a_generated_frame() {
    let (_gpu, device) = device();
    for size in [(1280, 720), (1920, 1080), (3840, 2160)] {
        let a = luma_texture(&device, &shifted(size, (0, 0)));
        let b = luma_texture(&device, &shifted(size, (13, -6)));
        let depth = depth_texture(&device, size, &[]);
        let mut passes = FrameGenPasses::default();
        let frame = |current, previous| Inputs {
            current,
            previous,
            depth: &depth,
            velocity: None,
            size,
            reset: false,
            debug: (false, false),
        };
        run(&device, &mut passes, &frame(&a, &b), false);
        run(&device, &mut passes, &frame(&b, &a), true);
        let wait = || {
            device
                .device
                .poll(wgpu::PollType::wait_indefinitely())
                .unwrap_or_else(|e| panic!("{e}"));
        };
        wait();
        let frames = 8;
        let start = std::time::Instant::now();
        for i in 0..frames {
            if i % 2 == 0 {
                run(&device, &mut passes, &frame(&a, &b), true);
            } else {
                run(&device, &mut passes, &frame(&b, &a), true);
            }
            wait();
        }
        let ms = start.elapsed().as_secs_f64() * 1000.0 / f64::from(frames);
        println!("frame_gen {}x{}: {ms:.2} ms per generated frame", size.0, size.1);
    }
}
