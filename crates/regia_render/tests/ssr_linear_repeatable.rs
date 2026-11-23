// SPDX-License-Identifier: MPL-2.0
//! The linear screen-space reflection march renders a held frame the same way every time
//! (ADR-5713), on a headless device, golden-free.
//!
//! On Mesa RADV (RX 7900 XTX, Mesa 26.2.6) the march fetched the hit's colour inside its loop, on
//! the path that breaks out of it, and the same still scene came back as two different images:
//! up to 136 of 6144 pixels changed between consecutive frames of one renderer, by up to
//! 3000 cd/m², and two fresh renderers disagreed in the same way. Depth, the lit colour and the
//! hit decision were the same in every frame; only the colour fetched on the breaking path
//! moved. The march now records the hit's pixel and reads its colour after the loop.
//!
//! - held frames of one renderer are bit-identical;
//! - fresh renderers render the same frame bit for bit;
//! - the march is live in the scene (the mirror floor differs from the frame without it), so the
//!   two checks above are not about an image no reflection reached;
//! - the same holds without the cubes, with rough walls, with a rougher floor, at another frame
//!   size and without IBL (the base shader was unrepeatable in each: 88 to 172 pixels over 16
//!   held frames on the RX 7900).

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabLocalLight, LabMaterial, LabMesh, LabPointLight,
    LabScene, PostSettings, RenderDevice,
};

const W: u32 = 96;
const H: u32 = 64;
const EV: f32 = 7.0;
const HALF: f32 = 3.0;
const HEIGHT: f32 = 3.0;
/// Frames rendered before the held frames are compared.
const WARM_FRAMES: u32 = 4;
/// Held frames compared with the first. On the RX 7900 the base shader's other image came up in
/// about one frame in five, so 32 frames all agree by chance about once in a thousand runs.
const HELD_FRAMES: u32 = 32;
/// Fresh renderers compared with the first.
const FRESH_RENDERERS: u32 = 8;
/// Held frames of each scene variant.
const VARIANT_FRAMES: u32 = 16;

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(d) => Some(d),
        Err(err) => {
            eprintln!("skip ssr_linear_repeatable: {err}");
            None
        }
    }
}

fn slab(center: [f32; 3], half: [f32; 3], material: usize) -> LabMesh {
    let (v, i) = regia_render::box_mesh(half);
    LabMesh {
        vertices: regia_render::translate_mesh(&v, center),
        indices: i,
        material,
        ..LabMesh::unskinned()
    }
}

/// A closed room with a mirror floor and mirror walls, a red and a blue cube, lit by one point
/// light: reflection rays that hit, miss and graze silhouettes all over the floor.
fn mirror_room() -> LabScene {
    let t = 0.1;
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 1.5, 2.6],
        target: [0.0, 0.2, -1.0],
        fov_y: std::f32::consts::FRAC_PI_3,
        near: 0.05,
        far: 20.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: EV,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: EV,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: vec![LabLocalLight::Point(LabPointLight {
            position: [0.5, 2.5, 0.5],
            color: [1.0; 3],
            intensity: 6000.0,
            radius: 12.0,
            cast_shadows: false,
            source_radius: None,
        })],
        materials: vec![
            LabMaterial {
                base_color: [0.95, 0.95, 0.95, 1.0],
                metallic: 1.0,
                roughness: 0.02,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.6, 0.6, 0.6, 1.0],
                metallic: 1.0,
                roughness: 0.05,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.8, 0.1, 0.08, 1.0],
                roughness: 0.7,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.08, 0.15, 0.8, 1.0],
                roughness: 0.7,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![
            slab([0.0, -t, 0.0], [HALF + t, t, HALF + t], 0),
            slab([0.0, HEIGHT + t, 0.0], [HALF + t, t, HALF + t], 1),
            slab([-HALF - t, HEIGHT / 2.0, 0.0], [t, HEIGHT / 2.0, HALF], 1),
            slab([HALF + t, HEIGHT / 2.0, 0.0], [t, HEIGHT / 2.0, HALF], 1),
            slab([0.0, HEIGHT / 2.0, -HALF - t], [HALF, HEIGHT / 2.0, t], 1),
            slab([0.0, HEIGHT / 2.0, HALF + t], [HALF, HEIGHT / 2.0, t], 1),
            slab([-1.4, 0.9, -1.4], [0.35; 3], 2),
            slab([1.4, 0.9, -1.4], [0.35; 3], 3),
        ],
    }
}

/// The lab baseline with the linear march (`ssr`) on or off; nothing temporal runs.
fn quality(ssr: bool) -> FrameQuality {
    let mut q = FrameQuality::LAB;
    q.taau = false;
    q.ssr = ssr;
    q.reflection_probes = true;
    q
}

/// One frame's linear HDR colour (the post stack's input, after the reflections).
fn hdr(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    frame: &regia_render::FrameScene<'_>,
) -> Vec<[f32; 3]> {
    renderer.request_hdr_capture();
    renderer.render_offscreen(device, frame).expect("frame");
    renderer
        .take_hdr_capture(device)
        .expect("readback")
        .expect("a post stack frame")
        .rgb
}

/// Pixels of `other` that differ from `first` in any bit, and the largest channel difference.
fn differing(first: &[[f32; 3]], other: &[[f32; 3]]) -> (usize, f32) {
    first
        .iter()
        .zip(other)
        .filter(|(a, b)| a.map(f32::to_bits) != b.map(f32::to_bits))
        .fold((0usize, 0.0f32), |(n, max), (a, b)| {
            let d = (0..3).map(|k| (a[k] - b[k]).abs()).fold(0.0f32, f32::max);
            (n + 1, max.max(d))
        })
}

/// FNV-1a over the frame's bits, printed so runs of two builds on one adapter can be compared
/// (the fix must not move a pixel where the march was already repeatable).
fn frame_hash(frame: &[[f32; 3]]) -> u64 {
    frame
        .iter()
        .flatten()
        .flat_map(|v| v.to_bits().to_le_bytes())
        .fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

/// Mean absolute difference over the bottom third of the image: the mirror floor in front of the
/// camera.
fn floor_mean_abs_diff(a: &[[f32; 3]], b: &[[f32; 3]]) -> f32 {
    let start = (H * 2 / 3 * W) as usize;
    let (sum, n) = a[start..]
        .iter()
        .zip(&b[start..])
        .fold((0.0f64, 0usize), |(s, n), (x, y)| {
            let d = (x[0] - y[0]).abs() + (x[1] - y[1]).abs() + (x[2] - y[2]).abs();
            (s + f64::from(d), n + 1)
        });
    (sum / n.max(1) as f64) as f32
}

#[test]
fn held_frames_of_the_linear_march_are_bit_identical() {
    let Some(device) = device() else { return };
    let lab = mirror_room();
    let frame = lab.as_frame_scene_with_quality(quality(true));
    let mut renderer = FrameRenderer::new();
    for _ in 0..WARM_FRAMES {
        renderer.render_offscreen(&device, &frame).expect("frame");
    }
    let first = hdr(&device, &mut renderer, &frame);
    let mut changed_frames = 0u32;
    let (mut worst_pixels, mut worst_diff) = (0usize, 0.0f32);
    for _ in 1..HELD_FRAMES {
        let again = hdr(&device, &mut renderer, &frame);
        let (pixels, diff) = differing(&first, &again);
        if pixels > 0 {
            changed_frames += 1;
        }
        worst_pixels = worst_pixels.max(pixels);
        worst_diff = worst_diff.max(diff);
    }
    eprintln!(
        "linear march, {HELD_FRAMES} held frames: {changed_frames} differ from the first, at most \
         {worst_pixels} of {} pixels, by up to {worst_diff}; first frame hash {:016x}",
        first.len(),
        frame_hash(&first)
    );
    assert_eq!(
        changed_frames, 0,
        "a held frame is not repeatable: {changed_frames} of {HELD_FRAMES} frames differ from the \
         first (at most {worst_pixels} pixels, by up to {worst_diff})"
    );

    // The march is live: without it the mirror floor is another image.
    let plain_frame = lab.as_frame_scene_with_quality(quality(false));
    let mut plain = FrameRenderer::new();
    for _ in 0..WARM_FRAMES {
        plain.render_offscreen(&device, &plain_frame).expect("frame");
    }
    let without = hdr(&device, &mut plain, &plain_frame);
    let effect = floor_mean_abs_diff(&first, &without);
    eprintln!("the march moves the mirror floor by {effect:.3}");
    assert!(effect > 0.5, "the march reflects on the mirror floor: {effect}");
}

#[test]
fn fresh_renderers_render_the_same_linear_march() {
    let Some(device) = device() else { return };
    let lab = mirror_room();
    let frame = lab.as_frame_scene_with_quality(quality(true));
    let render = || {
        let mut renderer = FrameRenderer::new();
        for _ in 1..WARM_FRAMES {
            renderer.render_offscreen(&device, &frame).expect("frame");
        }
        hdr(&device, &mut renderer, &frame)
    };
    let first = render();
    let mut changed = 0u32;
    let (mut worst_pixels, mut worst_diff) = (0usize, 0.0f32);
    for _ in 1..FRESH_RENDERERS {
        let (pixels, diff) = differing(&first, &render());
        if pixels > 0 {
            changed += 1;
        }
        worst_pixels = worst_pixels.max(pixels);
        worst_diff = worst_diff.max(diff);
    }
    eprintln!(
        "linear march, {FRESH_RENDERERS} fresh renderers: {changed} differ from the first, at most \
         {worst_pixels} of {} pixels, by up to {worst_diff}; first frame hash {:016x}",
        first.len(),
        frame_hash(&first)
    );
    assert_eq!(
        changed, 0,
        "fresh renderers disagree: {changed} of {FRESH_RENDERERS} differ from the first (at most \
         {worst_pixels} pixels, by up to {worst_diff})"
    );
}

/// A scene variant: its name and the change it makes to [`mirror_room`].
type Variant = (&'static str, fn(&mut LabScene));

#[test]
fn other_rooms_and_sizes_are_repeatable() {
    let Some(device) = device() else { return };
    let variants: [Variant; 5] = [
        ("no cubes", |lab| lab.meshes.truncate(6)),
        ("rough walls", |lab| {
            lab.materials[1].metallic = 0.0;
            lab.materials[1].roughness = 0.9;
        }),
        ("floor roughness 0.3", |lab| lab.materials[0].roughness = 0.3),
        ("128x72", |lab| {
            lab.width = 128;
            lab.height = 72;
        }),
        ("no ibl", |lab| lab.ibl = false),
    ];
    for (name, change) in variants {
        let mut lab = mirror_room();
        change(&mut lab);
        let frame = lab.as_frame_scene_with_quality(quality(true));
        let mut renderer = FrameRenderer::new();
        for _ in 0..WARM_FRAMES {
            renderer.render_offscreen(&device, &frame).expect("frame");
        }
        let first = hdr(&device, &mut renderer, &frame);
        let mut changed_frames = 0u32;
        let (mut worst_pixels, mut worst_diff) = (0usize, 0.0f32);
        for _ in 1..VARIANT_FRAMES {
            let (pixels, diff) = differing(&first, &hdr(&device, &mut renderer, &frame));
            if pixels > 0 {
                changed_frames += 1;
            }
            worst_pixels = worst_pixels.max(pixels);
            worst_diff = worst_diff.max(diff);
        }
        eprintln!(
            "linear march, {name}, {VARIANT_FRAMES} held frames: {changed_frames} differ from the first, \
             at most {worst_pixels} of {} pixels, by up to {worst_diff}; first frame hash {:016x}",
            first.len(),
            frame_hash(&first)
        );
        assert_eq!(
            changed_frames, 0,
            "{name}: a held frame is not repeatable: {changed_frames} of {VARIANT_FRAMES} frames differ from \
             the first (at most {worst_pixels} pixels, by up to {worst_diff})"
        );
    }
}
