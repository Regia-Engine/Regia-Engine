// SPDX-License-Identifier: MPL-2.0
//! Baked GI leak control end to end (ADR-1105): a path-traced bake of a sealed room on open
//! ground under a low sun. Its west wall is 10 cm thick and stands 0.25 m past the terrain
//! layer's x = 0 probe plane, the thin-wall layout of `regia_render`'s CPU repro
//! (`tests/baked_gi_thin_wall.rs`), so the sunlit probes just outside are the nearest corners of
//! the floor at the wall's foot inside.
//!
//! - Through the CPU mirror of the lookup on the real bake, the per-tap visibility with the
//!   light-bleeding reduction (the default) never reads more light inside than the legacy
//!   lookup, and less in sum.
//! - Rendered from inside the room, the floor at the wall reads no brighter with the default than
//!   with the legacy settings.
//!
//! Runs on whatever adapter `RenderDevice::headless` picks (lavapipe in CI).

use std::sync::{Arc, OnceLock};

use regia_asset::baked_gi::{BakedGiVolume, GI_COMPONENTS, TerrainLayer, VolumeLayout};
use regia_core::glam::{Mat4, Vec3};
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::probe::{GiBakeSettings, ProbeTraceSettings, bake_gi_volume};
use regia_pt::{PathTracer, PtScene};
use regia_render::{
    BakedGiSettings, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, box_mesh, sample_static_gi,
};

const W: u32 = 160;
const H: u32 = 96;

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| RenderDevice::headless().expect("baked GI needs a GPU; lavapipe is a GPU here"))
}

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| PathTracer::new(device()).expect("compile the path tracer"))
}

fn block(centre: [f32; 3], half: [f32; 3]) -> LabMesh {
    let (vertices, indices) = box_mesh(half);
    LabMesh {
        vertices,
        indices,
        material: 0,
        model: Mat4::from_translation(centre.into()).to_cols_array_2d(),
        ..LabMesh::unskinned()
    }
}

fn quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.render_scale_permille = 1000;
    q
}

/// The room: inside x 0.35..6.25, z −6.0..6.0, y 0..3.1, walls 0.1 m thick, a roof slab on top.
/// The probe rows z = −4, 0 and 4 run through it, clear of its north and south walls.
fn meshes() -> Vec<LabMesh> {
    vec![
        // Ground.
        block([0.0, -0.25, 0.0], [20.0, 0.25, 20.0]),
        // The thin west wall, x 0.25..0.35.
        block([0.3, 1.55, 0.0], [0.05, 1.55, 6.1]),
        // East wall, x 6.25..6.35.
        block([6.3, 1.55, 0.0], [0.05, 1.55, 6.1]),
        // North and south walls.
        block([3.3, 1.55, -6.05], [2.95, 1.55, 0.05]),
        block([3.3, 1.55, 6.05], [2.95, 1.55, 0.05]),
        // Roof, y 3.1..3.2.
        block([3.3, 3.15, 0.0], [3.05, 0.05, 6.1]),
    ]
}

fn lab(eye: [f32; 3], target: [f32; 3]) -> LabScene {
    let mut post = PostSettings::golden_deterministic();
    post.exposure_ev = 13.5;
    LabScene {
        width: W,
        height: H,
        eye,
        target,
        fov_y: 1.0,
        near: 0.05,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 13.5,
        ambient_lux: 8_000.0,
        double_sided: false,
        post: Some(post),
        // A low sun from the west lights the thin wall's outer face and the ground beside it.
        directional: DirectionalLightUniform::new([0.807, -0.555, 0.202], [1.0, 0.97, 0.92], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.6, 0.6, 0.6, 1.0],
            roughness: 0.95,
            ..LabMaterial::default()
        }],
        meshes: meshes(),
    }
}

/// Inside the room, looking west at the thin wall's foot.
fn inside() -> LabScene {
    lab([3.0, 1.6, 1.0], [0.3, 0.2, 0.5])
}

fn pt_scene(scene: &LabScene) -> PtScene {
    from_frame(
        &scene.as_frame_scene_with_quality(quality()),
        &ExtractOptions::default(),
    )
}

/// The terrain layer: columns 4 m apart from (−8, −8), so x = 0 is a probe plane 0.25 m west of
/// the thin wall and x = 4 runs through the room; levels 1, 2.5 and 5 m (the top one is above
/// the roof).
fn layout() -> VolumeLayout {
    let mut layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [-8.0, -8.0],
            spacing: [4.0, 4.0],
            counts: [6, 6],
            levels: vec![1.0, 2.5, 5.0],
            ground: vec![0.0; 36],
        }),
        bricks: Vec::new(),
        moment_resolution: 8,
        max_distance: 16.0,
    };
    layout.assign_brick_offsets();
    layout
}

fn volume() -> Arc<BakedGiVolume> {
    static V: OnceLock<Arc<BakedGiVolume>> = OnceLock::new();
    V.get_or_init(|| {
        let settings = GiBakeSettings {
            trace: ProbeTraceSettings {
                rays_per_probe: 128,
                samples: 8,
                max_bounces: 4,
                seed: 5,
                samples_per_submit: 8,
                ..ProbeTraceSettings::default()
            },
            ..GiBakeSettings::default()
        };
        let bake = bake_gi_volume(tracer(), &pt_scene(&inside()), &layout(), &settings, |_, _, _| {
            true
        })
        .expect("bake");
        eprintln!("baked {:?}", bake.report);
        Arc::new(bake.volume)
    })
    .clone()
}

fn lum(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// The baked light at `p` (normal `n`) seen from `eye`, at the baked light levels.
fn baked_at(vol: &BakedGiVolume, s: &BakedGiSettings, p: [f32; 3], n: [f32; 3], eye: [f32; 3]) -> f32 {
    let v = (Vec3::from_array(eye) - Vec3::from_array(p))
        .normalize()
        .to_array();
    let r = sample_static_gi(vol, s, &[[1.0; 3]; GI_COMPONENTS], p, n, v);
    lum(r.irradiance_over_pi)
}

fn linear_luma(pixels: &[u8], x0: u32, y0: u32, x1: u32, y1: u32) -> f32 {
    let lin = |c: u8| {
        let s = f32::from(c) / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    let (mut sum, mut n) = (0.0, 0.0);
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * W + x) * 4) as usize;
            sum += 0.2126 * lin(pixels[i]) + 0.7152 * lin(pixels[i + 1]) + 0.0722 * lin(pixels[i + 2]);
            n += 1.0;
        }
    }
    sum / n
}

#[test]
fn the_per_tap_lookup_reads_no_more_light_inside_than_the_legacy_one() {
    let vol = volume();
    let legacy = BakedGiSettings::legacy_visibility();
    let now = BakedGiSettings::default();
    let up = [0.0, 1.0, 0.0];
    let outdoor = baked_at(&vol, &now, [-2.0, 0.03, 1.0], up, [-5.0, 1.6, 1.0]);
    assert!(outdoor > 0.0, "the sunlit ground outside is lit: {outdoor}");
    let eye = inside().eye;
    let receivers = [
        ([0.45, 0.03, 1.0], up),
        ([0.8, 0.03, -1.3], up),
        ([0.6, 0.03, 2.1], up),
        ([1.5, 0.03, 0.4], up),
        ([0.36, 1.2, 0.2], [1.0, 0.0, 0.0]),
        ([0.36, 0.5, -0.7], [1.0, 0.0, 0.0]),
    ];
    let (mut sum_legacy, mut sum_now) = (0.0, 0.0);
    for (p, n) in receivers {
        let before = baked_at(&vol, &legacy, p, n, eye);
        let after = baked_at(&vol, &now, p, n, eye);
        eprintln!(
            "inside {p:?}: legacy {:.4} of the outdoor ground, per tap + reduction {:.4}",
            before / outdoor,
            after / outdoor
        );
        assert!(
            after <= before * 1.02 + 1e-3 * outdoor,
            "{p:?}: the new lookup reads more light inside ({after} vs {before})"
        );
        sum_legacy += before;
        sum_now += after;
    }
    eprintln!(
        "inside, summed: legacy {:.4}, now {:.4} of the outdoor ground",
        sum_legacy / outdoor,
        sum_now / outdoor
    );
    assert!(sum_now <= sum_legacy, "{sum_now} vs {sum_legacy}");
}

#[test]
fn the_floor_at_the_thin_wall_renders_no_brighter_with_the_default() {
    let scene = inside();
    let mut renderer = FrameRenderer::new();
    renderer.set_baked_gi(Some(volume()));
    let mut frame = |settings: BakedGiSettings| {
        renderer.set_baked_gi_settings(settings);
        let mut last = Vec::new();
        for _ in 0..2 {
            last = renderer
                .render_offscreen(device(), &scene.as_frame_scene_with_quality(quality()))
                .expect("render")
                .pixels;
        }
        last
    };
    let now = frame(BakedGiSettings::default());
    let legacy = frame(BakedGiSettings::legacy_visibility());
    assert!(renderer.baked_gi_status().active, "the volume lit the frame");
    // The lower half of the frame: the floor running up to the wall's foot.
    let a = linear_luma(&now, 16, 50, 144, 92);
    let b = linear_luma(&legacy, 16, 50, 144, 92);
    eprintln!("floor at the thin wall: default {a:.5}, legacy {b:.5}");
    assert!(a <= b * 1.02 + 1e-4, "default {a} vs legacy {b}");
}
