// SPDX-License-Identifier: MPL-2.0
//! Weighted blended order-independent transparency in the frame (ADR-2065): translucent meshes
//! composite as the model in [`regia_render::oit`] says, in any draw order, under glass and over
//! the opaque image, and the reactive mask reaches the temporal upscalers. With the knob off, or on
//! a frame that cannot run the pass, the frame does not change at all.
//!
//! The plates are unlit, so the image holds the materials' own colors and every expected value is
//! arithmetic on them.

use regia_render::oit::{OitFragment, composite, reactive_from_revealage};
use regia_render::{
    ALPHA_BLEND, ALPHA_PREMULTIPLIED, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial,
    LabMesh, LabScene, LabVertex, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
    SubsurfaceFactors, SubsurfaceProfile, SurfaceGraph, TemporalUpscaler,
};

const W: u32 = 160;
const H: u32 = 96;
const EYE_Z: f32 = 4.0;

/// Device creation and teardown one at a time (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> (std::sync::MutexGuard<'static, ()>, RenderDevice) {
    let guard = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    (guard, RenderDevice::headless().expect("headless device"))
}

/// A rectangle on the plane `z`, facing the eye.
fn plate(x: [f32; 2], y: [f32; 2], z: f32, material: usize) -> LabMesh {
    let n = [0.0, 0.0, 1.0];
    LabMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([x[0], y[0], z], n, [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([x[1], y[0], z], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([x[1], y[1], z], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([x[0], y[1], z], n, [0.0, 1.0]),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    }
}

/// A plate standing across the view, turned about the vertical through the origin: `slope` is how
/// far its z moves per unit of x.
fn crossing_plate(slope: f32, material: usize) -> LabMesh {
    let n = [-slope, 0.0, 1.0];
    let corner = |x: f32, y: f32| LabVertex::from_pos_normal_uv([x, y, slope * x], n, [0.0, 0.0]);
    LabMesh {
        vertices: vec![
            corner(-2.0, -1.0),
            corner(2.0, -1.0),
            corner(2.0, 1.0),
            corner(-2.0, 1.0),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    }
}

fn unlit(color: [f32; 3], alpha: f32, mode: u32) -> LabMaterial {
    LabMaterial {
        base_color: [color[0], color[1], color[2], alpha],
        alpha_mode: mode,
        unlit: true,
        double_sided: true,
        ..LabMaterial::default()
    }
}

const BACKGROUND: [f32; 3] = [0.2, 0.4, 0.6];

/// A graph that shades a half-covering matte surface.
const GRAPH_SURFACE: &str = r"
fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {
    return regia::pbr::surface::Surface(
        vec3<f32>(0.8, 0.5, 0.3),
        0.5,
        0.0,
        0.6,
        in.world_normal,
        1.0,
        vec3<f32>(0.0),
        0.0,
    );
}
";

/// Material 0 is the opaque backdrop on z = -2, across the whole view.
fn scene(mut materials: Vec<LabMaterial>, mut meshes: Vec<LabMesh>) -> LabScene {
    materials.insert(0, unlit(BACKGROUND, 1.0, regia_render::ALPHA_OPAQUE));
    meshes.insert(0, plate([-20.0, 20.0], [-20.0, 20.0], -2.0, 0));
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, EYE_Z],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.6,
        near: 0.1,
        far: 100.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 13.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            enabled: true,
            bloom: false,
            auto_exposure: false,
            taa: false,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([0.0, 0.0, -1.0], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials,
        meshes,
    }
}

fn quality(oit: bool) -> FrameQuality {
    FrameQuality {
        order_independent_transparency: oit,
        specular_aa: false,
        ..FrameQuality::LAB
    }
}

fn hdr(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    lab: &LabScene,
    quality: FrameQuality,
) -> Vec<[f32; 3]> {
    let scene = lab.as_frame_scene_with_quality(quality);
    renderer.request_hdr_capture();
    renderer.render_offscreen(device, &scene).expect("frame");
    renderer
        .take_hdr_capture(device)
        .expect("hdr readback")
        .expect("the post stack ran, so the HDR copy exists")
        .rgb
}

fn at(image: &[[f32; 3]], x: u32, y: u32) -> [f32; 3] {
    image[(y * W + x) as usize]
}

fn worst(a: &[[f32; 3]], b: &[[f32; 3]]) -> f32 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .flat_map(|(x, y)| x.iter().zip(y).map(|(p, q)| (p - q).abs()))
        .fold(0.0, f32::max)
}

fn near(a: [f32; 3], b: [f32; 3], tolerance: f32) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() <= tolerance)
}

/// The red plane of the frame's reactive mask.
fn read_mask(device: &RenderDevice, renderer: &FrameRenderer) -> (u32, u32, Vec<u8>) {
    let view = renderer.oit_reactive_mask().expect("the frame wrote a mask");
    let texture = view.texture();
    let size = texture.size();
    let (width, height) = (size.width, size.height);
    let padded = width.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("oit mask readback"),
        size: u64::from(padded) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("oit mask readback"),
        });
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
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        size,
    );
    device.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    device
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll readback");
    rx.recv().expect("map callback").expect("map readback");
    let mapped = buffer.slice(..).get_mapped_range().expect("mapped range");
    let mut out = Vec::with_capacity((width * height) as usize);
    for y in 0..height as usize {
        out.extend_from_slice(&mapped[y * padded as usize..y * padded as usize + width as usize]);
    }
    (width, height, out)
}

/// One translucent layer: the "over" operator exactly, so the frame matches the sorted one for an
/// alpha-blended and a premultiplied material. The pixels outside the plate do not change at all.
#[test]
fn a_single_layer_matches_sorted_blending() {
    let (_gpu, device) = device();
    for (mode, color) in [
        (ALPHA_BLEND, [0.9, 0.3, 0.1]),
        (ALPHA_PREMULTIPLIED, [0.45, 0.15, 0.05]),
    ] {
        let lab = scene(
            vec![unlit(color, 0.5, mode)],
            vec![plate([-0.5, 0.5], [-0.5, 0.5], 0.0, 1)],
        );
        let mut renderer = FrameRenderer::new();
        let off = hdr(&device, &mut renderer, &lab, quality(false));
        assert_eq!(renderer.oit_stats().reason, Some("profile_off"));
        assert!(renderer.oit_reactive_mask().is_none());
        let on = hdr(&device, &mut renderer, &lab, quality(true));
        let stats = renderer.oit_stats();
        assert_eq!(stats.reason, None, "{stats:?}");
        assert_eq!(
            (stats.drawn, stats.fallback, stats.sorted),
            (1, 0, 0),
            "{stats:?}"
        );
        assert_eq!(stats.passes, 1);
        assert!(stats.reactive_mask);
        let difference = worst(&off, &on);
        eprintln!("mode {mode}: worst difference against sorted {difference}");
        assert!(difference < 2.0e-3, "mode {mode}: {difference}");
        assert_eq!(at(&on, 2, 2), at(&off, 2, 2), "outside the plate");
        assert!(at(&on, W / 2, H / 2) != at(&on, 2, 2), "the plate shows");
    }
}

/// A lit translucent surface too: the same light through the variant's two targets, and the fog and
/// exposure the shader applies come along.
#[test]
fn a_lit_layer_matches_sorted_blending() {
    let (_gpu, device) = device();
    let lit = LabMaterial {
        base_color: [0.8, 0.5, 0.3, 0.6],
        alpha_mode: ALPHA_BLEND,
        roughness: 0.6,
        ..LabMaterial::default()
    };
    let mut lab = scene(vec![lit], vec![plate([-0.5, 0.5], [-0.5, 0.5], 0.0, 1)]);
    lab.directional_shadows = false;
    let mut renderer = FrameRenderer::new();
    let off = hdr(&device, &mut renderer, &lab, quality(false));
    let on = hdr(&device, &mut renderer, &lab, quality(true));
    let top = off.iter().flatten().fold(0.0f32, |top, v| top.max(*v));
    let difference = worst(&off, &on);
    eprintln!("lit layer: worst difference {difference} of a top value {top}");
    assert_eq!(renderer.oit_stats().drawn, 1);
    assert!(difference < 2.0e-3 * top.max(1.0), "{difference} of {top}");
}

/// A material graph takes the pass through its own variant: the frame matches the sorted one.
#[test]
fn a_translucent_material_graph_matches_sorted_blending() {
    let (_gpu, device) = device();
    let material = LabMaterial {
        base_color: [0.8, 0.5, 0.3, 0.5],
        alpha_mode: ALPHA_BLEND,
        roughness: 0.6,
        surface_graph: Some(std::sync::Arc::new(SurfaceGraph::new(GRAPH_SURFACE))),
        ..LabMaterial::default()
    };
    let lab = scene(vec![material], vec![plate([-0.5, 0.5], [-0.5, 0.5], 0.0, 1)]);
    let mut renderer = FrameRenderer::new();
    let off = hdr(&device, &mut renderer, &lab, quality(false));
    let on = hdr(&device, &mut renderer, &lab, quality(true));
    let stats = renderer.oit_stats();
    assert_eq!((stats.drawn, stats.fallback), (1, 0), "{stats:?}");
    let top = off.iter().flatten().fold(0.0f32, |top, v| top.max(*v));
    let difference = worst(&off, &on);
    eprintln!("graph: worst difference {difference} of {top}");
    assert!(difference < 2.0e-3 * top.max(1.0), "{difference} of {top}");
}

/// Far and faint: the weight's floors meet, and the frame still matches the sorted one.
#[test]
fn a_far_faint_layer_matches_sorted_blending() {
    let (_gpu, device) = device();
    let mut lab = scene(
        vec![unlit([0.9, 0.3, 0.1], 0.05, ALPHA_BLEND)],
        vec![plate([-200.0, 200.0], [-200.0, 200.0], -500.0, 1)],
    );
    lab.far = 5_000.0;
    lab.meshes[0] = plate([-2_000.0, 2_000.0], [-2_000.0, 2_000.0], -600.0, 0);
    let mut renderer = FrameRenderer::new();
    let off = hdr(&device, &mut renderer, &lab, quality(false));
    let on = hdr(&device, &mut renderer, &lab, quality(true));
    assert_eq!(renderer.oit_stats().drawn, 1);
    let difference = worst(&off, &on);
    let (center, base) = (at(&on, W / 2, H / 2), BACKGROUND);
    eprintln!("far and faint: worst difference {difference}, center {center:?}");
    assert!(difference < 2.0e-3, "{difference}");
    assert!(
        (center[0] - (0.05 * 0.9 + 0.95 * base[0])).abs() < 2.0e-3,
        "{center:?}"
    );
}

/// The screen-space passes read the image's alpha after the composite. A Desktop High frame with its
/// reflections and indirect light on matches the sorted frame, over a glossy floor.
#[test]
fn a_desktop_frame_with_screen_space_passes_matches_sorted_blending() {
    let (_gpu, device) = device();
    let floor = LabMaterial {
        base_color: [0.3, 0.3, 0.32, 1.0],
        roughness: 0.15,
        metallic: 0.0,
        ..LabMaterial::default()
    };
    let glossy_plate = LabMaterial {
        base_color: [0.8, 0.5, 0.3, 0.8],
        alpha_mode: ALPHA_BLEND,
        roughness: 0.5,
        double_sided: true,
        ..LabMaterial::default()
    };
    let (floor_v, floor_i) = regia_render::box_mesh([3.0, 0.05, 3.0]);
    let mut lab = scene(
        vec![floor, glossy_plate],
        vec![
            LabMesh {
                vertices: regia_render::translate_mesh(&floor_v, [0.0, -1.0, 0.0]),
                indices: floor_i,
                material: 1,
                ..LabMesh::unskinned()
            },
            plate([-0.5, 0.5], [-1.0, 0.2], 0.5, 2),
        ],
    );
    lab.depth_prepass = true;
    lab.eye = [0.0, 0.6, EYE_Z];
    lab.directional = DirectionalLightUniform::new([0.3, -0.8, -0.4], [1.0; 3], 400.0);
    let q = |oit: bool| {
        let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
        q.taau = false;
        q.order_independent_transparency = oit;
        q
    };
    assert!(
        q(true).ssr,
        "the preset must run the reflections for this to mean anything"
    );
    let run = |oit: bool| {
        // A fresh renderer each time, so the two frames share their frame index and its noise.
        let mut renderer = FrameRenderer::new();
        let mut image = Vec::new();
        for _ in 0..3 {
            image = hdr(&device, &mut renderer, &lab, q(oit));
        }
        assert_eq!(renderer.oit_stats().reason.is_none(), oit);
        image
    };
    let (off, control, on) = (run(false), run(false), run(true));
    let top = off.iter().flatten().fold(0.0f32, |top, v| top.max(*v));
    let (noise, difference) = (worst(&off, &control), worst(&off, &on));
    let at_worst = off
        .iter()
        .zip(&on)
        .enumerate()
        .max_by(|(_, a), (_, b)| {
            let gap = |p: &(&[f32; 3], &[f32; 3])| (p.0[0] - p.1[0]).abs().max((p.0[1] - p.1[1]).abs());
            gap(&(a.0, a.1)).total_cmp(&gap(&(b.0, b.1)))
        })
        .map(|(i, (a, b))| (i as u32 % W, i as u32 / W, *a, *b));
    eprintln!(
        "desktop: top {top}, the same frame twice {noise}, against sorted {difference}, worst at {at_worst:?}"
    );
    assert!(
        difference < noise + 5.0e-3 * top.max(1.0),
        "{difference} of {top}, noise {noise}"
    );
}

/// A frame the pass has nothing to do in is the same frame to the byte, and takes no target.
#[test]
fn a_frame_with_no_translucent_mesh_is_byte_identical_and_takes_nothing() {
    let (_gpu, device) = device();
    let lab = scene(
        vec![unlit([0.9, 0.2, 0.2], 1.0, regia_render::ALPHA_OPAQUE)],
        vec![plate([-0.5, 0.5], [-0.5, 0.5], 0.0, 1)],
    );
    let mut off_renderer = FrameRenderer::new();
    let off = off_renderer
        .render_offscreen(&device, &lab.as_frame_scene_with_quality(quality(false)))
        .expect("frame")
        .pixels;
    let targets = off_renderer.graph_report().graph.transients.created;
    let mut on_renderer = FrameRenderer::new();
    let on = on_renderer
        .render_offscreen(&device, &lab.as_frame_scene_with_quality(quality(true)))
        .expect("frame")
        .pixels;
    assert_eq!(on_renderer.oit_stats().reason, Some("no_material"));
    assert!(off == on, "the frame moved");
    assert_eq!(on_renderer.graph_report().graph.transients.created, targets);
    assert!(on_renderer.oit_reactive_mask().is_none());
}

/// Two plates through each other, the case no mesh order draws. Sorted, the order of the list shows
/// (the proof the plate is the hard case). With the pass, the order changes nothing.
#[test]
fn crossing_surfaces_do_not_depend_on_the_draw_order() {
    let (_gpu, device) = device();
    let red = unlit([1.0, 0.0, 0.0], 0.6, ALPHA_BLEND);
    let green = unlit([0.0, 1.0, 0.0], 0.6, ALPHA_BLEND);
    let lab = |first: [LabMesh; 2]| scene(vec![red.clone(), green.clone()], first.into_iter().collect());
    let a = lab([crossing_plate(0.5, 1), crossing_plate(-0.5, 2)]);
    let b = lab([crossing_plate(-0.5, 2), crossing_plate(0.5, 1)]);
    let mut renderer = FrameRenderer::new();
    let sorted_a = hdr(&device, &mut renderer, &a, quality(false));
    let sorted_b = hdr(&device, &mut renderer, &b, quality(false));
    let oit_a = hdr(&device, &mut renderer, &a, quality(true));
    let oit_b = hdr(&device, &mut renderer, &b, quality(true));
    assert_eq!(renderer.oit_stats().drawn, 2);
    let sorted_gap = worst(&sorted_a, &sorted_b);
    let oit_gap = worst(&oit_a, &oit_b);
    eprintln!("order changes the frame by {sorted_gap} sorted, {oit_gap} with the pass");
    assert!(
        sorted_gap > 0.05,
        "sorted {sorted_gap}: the plates must show the problem"
    );
    assert!(oit_gap < 1.0e-3, "with the pass {oit_gap}");

    // Each side shows the nearer plate's color more: right of the crossing, red is in front.
    let k = at(&oit_a, 2, 2)[2] / BACKGROUND[2];
    let (right, left) = (at(&oit_a, W / 2 + 25, H / 2), at(&oit_a, W / 2 - 25, H / 2));
    assert!(right[0] > right[1], "right {right:?}, scale {k}");
    assert!(left[1] > left[0], "left {left:?}, scale {k}");
}

/// Two parallel layers against the CPU model: the frame and [`composite`] agree, and how much of
/// the background shows is the exact product.
#[test]
fn two_layers_match_the_model() {
    let (_gpu, device) = device();
    let (near_color, far_color) = ([0.9, 0.1, 0.1], [0.1, 0.8, 0.2]);
    let lab = scene(
        vec![
            unlit(near_color, 0.5, ALPHA_BLEND),
            unlit(far_color, 0.3, ALPHA_BLEND),
        ],
        vec![
            plate([-0.5, 0.5], [-0.5, 0.5], 0.5, 1),
            plate([-0.5, 0.5], [-0.5, 0.5], -1.0, 2),
        ],
    );
    let mut renderer = FrameRenderer::new();
    let image = hdr(&device, &mut renderer, &lab, quality(true));
    let k = at(&image, 2, 2)[0] / BACKGROUND[0];
    let layer = |color: [f32; 3], alpha: f32, z: f32| OitFragment {
        color,
        alpha,
        premultiplied: false,
        distance: EYE_Z - z,
    };
    let model = composite(
        BACKGROUND,
        &[layer(near_color, 0.5, 0.5), layer(far_color, 0.3, -1.0)],
    );
    let center = at(&image, W / 2, H / 2).map(|v| v / k);
    eprintln!("center {center:?}, model {model:?}");
    assert!(near(center, model, 2.0e-3), "center {center:?}, model {model:?}");

    // The reactive mask at the center holds the covered share.
    let (width, _, mask) = read_mask(&device, &renderer);
    let value = f32::from(mask[(H / 2 * width + W / 2) as usize]) / 255.0;
    let expected = reactive_from_revealage(0.5 * 0.7);
    assert!(
        (value - expected).abs() <= 1.5 / 255.0,
        "{value} against {expected}"
    );
}

/// A translucent plate behind an opaque one does not show, with and without a depth prepass.
#[test]
fn an_opaque_surface_hides_what_is_behind_it() {
    let (_gpu, device) = device();
    for prepass in [false, true] {
        let wall = unlit([0.7, 0.7, 0.7], 1.0, regia_render::ALPHA_OPAQUE);
        let mut hidden = scene(
            vec![wall.clone(), unlit([1.0, 0.0, 0.0], 0.8, ALPHA_BLEND)],
            vec![
                plate([-1.0, 1.0], [-1.0, 1.0], 0.0, 1),
                plate([-0.5, 0.5], [-0.5, 0.5], -1.0, 2),
            ],
        );
        hidden.depth_prepass = prepass;
        let mut without = scene(vec![wall], vec![plate([-1.0, 1.0], [-1.0, 1.0], 0.0, 1)]);
        without.depth_prepass = prepass;
        let mut renderer = FrameRenderer::new();
        let plain = hdr(&device, &mut renderer, &without, quality(false));
        let image = hdr(&device, &mut renderer, &hidden, quality(true));
        assert_eq!(renderer.oit_stats().drawn, 1);
        assert_eq!(worst(&plain, &image), 0.0, "prepass {prepass}");
    }
}

/// Glass stays in the sorted run and draws over the composite, so a translucent plate behind a pane
/// looks as it does sorted. An additive plate keeps the sorted run too.
#[test]
fn glass_and_additive_draws_keep_the_sorted_run_over_the_composite() {
    let (_gpu, device) = device();
    let pane = LabMaterial {
        base_color: [0.6, 0.9, 0.7, 1.0],
        alpha_mode: ALPHA_PREMULTIPLIED,
        transmission: 0.9,
        roughness: 0.3,
        ..LabMaterial::default()
    };
    let mut lab = scene(
        vec![
            unlit([1.0, 0.2, 0.1], 0.5, ALPHA_BLEND),
            pane,
            unlit([0.1, 0.1, 0.3], 1.0, regia_render::ALPHA_ADDITIVE),
        ],
        vec![
            plate([-0.7, 0.7], [-0.7, 0.7], -0.5, 1),
            plate([-0.8, 0.8], [-0.8, 0.8], 1.0, 2),
            plate([-0.3, 0.3], [-0.3, 0.3], 1.5, 3),
        ],
    );
    // A dim sun: the pane's own light stays in the range of the unlit plates. Under a full sun it
    // reaches the thousands, where a half float's rounding would hide the plate from this test.
    lab.directional = DirectionalLightUniform::new([0.0, 0.0, -1.0], [1.0; 3], 5.0);
    let mut without = lab.clone();
    without.meshes.remove(1);
    let mut renderer = FrameRenderer::new();
    let bare = hdr(&device, &mut renderer, &without, quality(true));
    assert_eq!(renderer.oit_stats().reason, Some("no_material"));
    let off = hdr(&device, &mut renderer, &lab, quality(false));
    let on = hdr(&device, &mut renderer, &lab, quality(true));
    let stats = renderer.oit_stats();
    assert_eq!(
        (stats.drawn, stats.sorted, stats.fallback),
        (1, 2, 0),
        "{stats:?}"
    );
    let difference = worst(&off, &on);
    // Beside the additive plate, inside the translucent one, under the pane.
    let (x, y) = (W / 2 + 22, H / 2);
    let shown = (at(&on, x, y)[0] - at(&bare, x, y)[0]).abs();
    eprintln!(
        "under glass and additive: worst difference against sorted {difference}, the plate shows by {shown}"
    );
    assert!(shown > 0.02, "the plate must show through the pane: {shown}");
    assert!(difference < 3.0e-3, "{difference}");
}

/// A frame that cannot run the pass draws sorted, says why, and is the same frame.
#[test]
fn a_frame_that_cannot_run_the_pass_is_the_sorted_frame_and_says_why() {
    let (_gpu, device) = device();
    let lab = scene(
        vec![unlit([0.9, 0.3, 0.1], 0.5, ALPHA_BLEND)],
        vec![plate([-0.5, 0.5], [-0.5, 0.5], 0.0, 1)],
    );
    let pixels = |renderer: &mut FrameRenderer, lab: &LabScene, q: FrameQuality| {
        renderer
            .render_offscreen(&device, &lab.as_frame_scene_with_quality(q))
            .expect("frame")
            .pixels
    };

    // Multisampled.
    let msaa = |oit: bool| FrameQuality {
        msaa_samples: 4,
        ..quality(oit)
    };
    let mut renderer = FrameRenderer::new();
    let off = pixels(&mut renderer, &lab, msaa(false));
    let on = pixels(&mut renderer, &lab, msaa(true));
    let stats = renderer.oit_stats();
    assert_eq!(stats.reason, Some("msaa"), "{stats:?}");
    assert_eq!((stats.drawn, stats.fallback), (0, 1), "{stats:?}");
    assert!(off == on, "msaa frame moved");

    // No HDR image: no post stack and no temporal upscale.
    let mut plain = scene(
        vec![unlit([0.9, 0.3, 0.1], 0.5, ALPHA_BLEND)],
        vec![plate([-0.5, 0.5], [-0.5, 0.5], 0.0, 1)],
    );
    plain.post = None;
    let mut renderer = FrameRenderer::new();
    let off = pixels(&mut renderer, &plain, quality(false));
    let on = pixels(&mut renderer, &plain, quality(true));
    assert_eq!(renderer.oit_stats().reason, Some("no_hdr_target"));
    assert!(off == on, "display-referred frame moved");

    // A Mobile lite tier.
    let mut renderer = FrameRenderer::new();
    renderer.set_forward_lite(Some(regia_render::forward_lite::ForwardLite::Low));
    let off = pixels(&mut renderer, &lab, quality(false));
    let on = pixels(&mut renderer, &lab, quality(true));
    let stats = renderer.oit_stats();
    assert_eq!(stats.reason, Some("forward_lite"), "{stats:?}");
    assert!(off == on, "lite frame moved");
}

/// The mask is written at the render size, 0 where nothing translucent is and the covered share
/// where it is, and follows a resize.
#[test]
fn the_reactive_mask_covers_the_translucent_pixels_at_the_render_size() {
    let (_gpu, device) = device();
    let mut lab = scene(
        vec![unlit([0.9, 0.3, 0.1], 0.5, ALPHA_BLEND)],
        vec![plate([-0.5, 0.5], [-0.5, 0.5], 0.0, 1)],
    );
    let mut renderer = FrameRenderer::new();
    let image = hdr(&device, &mut renderer, &lab, quality(true));
    assert_eq!(image.len(), (W * H) as usize);
    let (width, height, mask) = read_mask(&device, &renderer);
    assert_eq!((width, height), (W, H));
    let expected = (reactive_from_revealage(0.5) * 255.0).round();
    assert!((f32::from(mask[(H / 2 * W + W / 2) as usize]) - expected).abs() <= 1.0);
    assert_eq!(mask[0], 0, "outside the plate");
    assert_eq!(mask[(H / 2 * W + 2) as usize], 0, "outside the plate");
    assert!(mask.iter().filter(|&&v| v > 0).count() < (W * H) as usize / 2);

    lab.width = W * 2;
    lab.height = H * 2;
    renderer
        .render_offscreen(&device, &lab.as_frame_scene_with_quality(quality(true)))
        .expect("resized frame");
    let (width, height, mask) = read_mask(&device, &renderer);
    assert_eq!((width, height), (W * 2, H * 2));
    assert!((f32::from(mask[(H * W * 2 + W) as usize]) - expected).abs() <= 1.0);

    // Off again, the frame has no mask for anyone to read.
    renderer
        .render_offscreen(&device, &lab.as_frame_scene_with_quality(quality(false)))
        .expect("frame off");
    assert!(renderer.oit_reactive_mask().is_none());
}

/// The knob off, the pool lets the pass's targets go after its idle frames.
#[test]
fn switching_off_releases_the_targets() {
    let (_gpu, device) = device();
    let lab = scene(
        vec![unlit([0.9, 0.3, 0.1], 0.5, ALPHA_BLEND)],
        vec![plate([-0.5, 0.5], [-0.5, 0.5], 0.0, 1)],
    );
    let mut renderer = FrameRenderer::new();
    let frame = |renderer: &mut FrameRenderer, oit: bool| {
        renderer
            .render_offscreen(&device, &lab.as_frame_scene_with_quality(quality(oit)))
            .expect("frame");
    };
    frame(&mut renderer, false);
    let sorted = renderer.graph_report().graph.transients.textures;
    frame(&mut renderer, true);
    let with_pass = renderer.graph_report().graph.transients.textures;
    assert_eq!(with_pass, sorted + 3);
    for _ in 0..8 {
        frame(&mut renderer, false);
    }
    assert_eq!(renderer.graph_report().graph.transients.textures, sorted);
}

/// A held view's cached frame does not stand in across the switch: the frame changes at once.
#[test]
fn a_held_view_follows_the_knob() {
    let (_gpu, device) = device();
    let red = unlit([1.0, 0.0, 0.0], 0.6, ALPHA_BLEND);
    let green = unlit([0.0, 1.0, 0.0], 0.6, ALPHA_BLEND);
    let lab = scene(
        vec![red, green],
        vec![crossing_plate(0.5, 1), crossing_plate(-0.5, 2)],
    );
    let q = |oit: bool| {
        let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
        q.order_independent_transparency = oit;
        q
    };
    assert!(
        q(true).reuse_static_screen(),
        "the preset must hold a static view"
    );
    let mut renderer = FrameRenderer::new();
    let mut held = Vec::new();
    for _ in 0..24 {
        held = hdr(&device, &mut renderer, &lab, q(false));
    }
    let switched = hdr(&device, &mut renderer, &lab, q(true));
    assert_eq!(renderer.oit_stats().drawn, 2);
    let moved = worst(&held, &switched);
    eprintln!("the switch moves a held view by {moved}");
    assert!(moved > 0.02, "{moved}");
}

/// Black and white stripes behind a half-covering plate: the clip box stays wide over the whole
/// plate, so the history alone decides how fast a change in the plate's color shows.
fn stripes_scene(plate_color: f32, taa: bool) -> LabScene {
    let black = unlit([0.0; 3], 1.0, regia_render::ALPHA_OPAQUE);
    let white = unlit([1.0; 3], 1.0, regia_render::ALPHA_OPAQUE);
    let mut meshes = Vec::new();
    // The view's width at the stripes: 2 px wide bars.
    let view_h = 2.0 * (0.3f32).tan() * (EYE_Z + 1.0);
    let bar = view_h / H as f32 * 2.0;
    for i in 0..W as i32 {
        let x = (i - W as i32 / 2) as f32 * bar;
        meshes.push(plate([x, x + bar], [-20.0, 20.0], -1.0, 1 + (i % 2) as usize));
    }
    meshes.push(plate([-0.6, 0.6], [-0.6, 0.6], 0.0, 3));
    let mut lab = scene(
        vec![black, white, unlit([plate_color; 3], 0.6, ALPHA_BLEND)],
        meshes,
    );
    lab.depth_prepass = true;
    if let Some(post) = lab.post.as_mut() {
        post.taa = taa;
    }
    lab
}

fn taa_quality(oit: bool) -> FrameQuality {
    FrameQuality {
        order_independent_transparency: oit,
        taau: true,
        specular_aa: false,
        ..FrameQuality::LAB
    }
}

/// The plate's mean green over its middle: the stripes' jitter averages out, the lag does not.
fn plate_mean(image: &[[f32; 3]]) -> f32 {
    let mut sum = 0.0;
    for y in H / 2 - 8..H / 2 + 8 {
        for x in W / 2 - 8..W / 2 + 8 {
            sum += at(image, x, y)[1];
        }
    }
    sum / (16.0 * 16.0)
}

/// How far the plate is from its true color, on average over the six frames after the color changes.
/// The truth is arithmetic, not a later frame: a sorted plate over TSR is still on its way 24 frames on.
fn lag(device: &RenderDevice, upscaler: TemporalUpscaler, oit: bool) -> f32 {
    let mut renderer = FrameRenderer::new();
    renderer.set_temporal_upscaler(upscaler);
    let q = taa_quality(oit);
    let before = stripes_scene(0.2, true);
    let after = stripes_scene(0.5, true);
    for _ in 0..12 {
        hdr(device, &mut renderer, &before, q);
    }
    // The plate covers 0.6 of the stripes, whose mean is 0.5.
    let truth = 0.6 * 0.5 + 0.4 * 0.5;
    let mut sum = 0.0;
    for _ in 0..6 {
        sum += (plate_mean(&hdr(device, &mut renderer, &after, q)) - truth).abs();
    }
    if oit {
        let stats = renderer.oit_stats();
        assert_eq!(stats.reason, None, "{upscaler:?}: {stats:?}");
        assert!(stats.reactive_mask);
    }
    sum / 6.0
}

/// The mask reaches each built-in temporal upscaler: the same scene takes up the plate's new color
/// faster with the pass than sorted, where nothing tells the upscaler the plate is translucent. The
/// frames differ only by the mask (a single layer matches the sorted frame to a half float's step).
#[test]
fn the_built_in_upscalers_read_the_mask() {
    let (_gpu, device) = device();
    let mut failed = Vec::new();
    for upscaler in [
        TemporalUpscaler::Taau,
        TemporalUpscaler::Tsr,
        TemporalUpscaler::Fsr3,
    ] {
        let sorted = lag(&device, upscaler, false);
        let reactive = lag(&device, upscaler, true);
        eprintln!("{upscaler:?}: lag sorted {sorted}, with the mask {reactive}");
        if sorted <= 0.01 || reactive >= 0.8 * sorted {
            failed.push((upscaler, sorted, reactive));
        }
    }
    assert!(
        failed.is_empty(),
        "(upscaler, sorted lag, lag with the mask): {failed:?}"
    );
}

/// Scattering and the pass in one frame: their pooled targets share a format and neither disturbs
/// the other. The plate over the scattering surface composites over the finished skin.
#[test]
fn scattering_and_the_pass_share_a_frame() {
    let (_gpu, device) = device();
    let skin = LabMaterial {
        base_color: [0.8, 0.6, 0.5, 1.0],
        roughness: 1.0,
        subsurface: SubsurfaceFactors::new(SubsurfaceProfile::Skin),
        ..LabMaterial::default()
    };
    let mut lab = scene(
        vec![skin, unlit([0.1, 0.8, 0.2], 0.5, ALPHA_BLEND)],
        vec![
            plate([-1.0, 1.0], [-1.0, 1.0], 0.0, 1),
            plate([-0.5, 0.5], [-0.5, 0.5], 0.5, 2),
        ],
    );
    // A dim sun: the skin stays in the range of the unlit plate, so a half-float rounding at
    // thousands cannot hide the plate.
    lab.directional = DirectionalLightUniform::new([0.0, 0.0, -1.0], [1.0; 3], 5.0);
    let both = |oit: bool| FrameQuality {
        subsurface_scattering: true,
        ..quality(oit)
    };
    let mut renderer = FrameRenderer::new();
    let off = hdr(&device, &mut renderer, &lab, both(false));
    let on = hdr(&device, &mut renderer, &lab, both(true));
    assert_eq!(renderer.subsurface_stats().drawn, 1);
    assert_eq!(renderer.oit_stats().drawn, 1);
    let difference = worst(&off, &on);
    // The plate over the finished skin is "over" on the skin beside it (the plate is flat and evenly lit).
    let (skin_px, over_px) = (at(&on, W / 2 + 30, H / 2), at(&on, W / 2, H / 2));
    let expected = [
        0.5 * 0.1 + 0.5 * skin_px[0],
        0.5 * 0.8 + 0.5 * skin_px[1],
        0.5 * 0.2 + 0.5 * skin_px[2],
    ];
    eprintln!(
        "with scattering: worst difference {difference}, skin {skin_px:?}, under the plate {over_px:?}"
    );
    assert!(near(over_px, expected, 0.01), "{over_px:?} against {expected:?}");
    assert!(difference < 2.0e-3, "{difference}");
}

/// The desktop presets opt in by the one knob, whatever else they turn on.
#[test]
fn the_desktop_presets_can_opt_in() {
    let (_gpu, device) = device();
    let lab = scene(
        vec![unlit([0.9, 0.3, 0.1], 0.5, ALPHA_BLEND)],
        vec![plate([-0.5, 0.5], [-0.5, 0.5], 0.0, 1)],
    );
    for preset in [
        RenderProfilePreset::DesktopHigh,
        RenderProfilePreset::DesktopUltra,
    ] {
        let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
        q.order_independent_transparency = true;
        let mut renderer = FrameRenderer::new();
        for _ in 0..2 {
            renderer
                .render_offscreen(&device, &lab.as_frame_scene_with_quality(q))
                .expect("frame");
        }
        let stats = renderer.oit_stats();
        assert_eq!(stats.reason, None, "{preset:?}: {stats:?}");
        assert_eq!(stats.drawn, 1, "{preset:?}");
        assert!(
            stats.covered_permille > 0 && stats.covered_permille < 1000,
            "{stats:?}"
        );
    }
}
