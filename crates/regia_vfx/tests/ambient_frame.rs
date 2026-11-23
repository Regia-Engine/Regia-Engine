// SPDX-License-Identifier: MPL-2.0
//! Ambient life through the real frame path (ADR-3024), on the headless adapter (lavapipe in CI):
//! off draws the very frame a host without it draws; on, each kind changes pixels where it should
//! (leaves in their season's colors, birds as dark shapes against the bright backdrop, fireflies
//! only adding light); the same state draws the same bits. No golden images: the oracles are
//! pixel differences against the frame without the layer.

use regia_test_utils::serial_gpu::{SerialGpu, serial_device};
use std::sync::{Arc, Mutex};

use regia_core::math::Vec3;
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    QualityTier, RenderDevice, SurfaceWeatherFrame,
};
use regia_vfx::ambient::{
    AmbientBudget, AmbientCounts, AmbientIntegrator, AmbientLifeGpu, AmbientState, BirdFlockSim,
    BirdInstance, MAX_BIRDS, butterfly_at, butterfly_wing_angle,
};
use regia_vfx::precip::{PrecipState, PrecipitationGpu};
use regia_vfx::runtime::translucent_layer_with_ambient_life;
use regia_vfx::{
    AmbientBirds, AmbientLifeHandle, PrecipBudget, PrecipIntegrator, PrecipitationHandle,
    PrecipitationSettings, Season, VfxGpu, VfxProfile, WeatherConfig, WeatherState,
};

const W: u32 = 480;
const H: u32 = 270;
/// `a_birds_edge_is_a_pixel_wide`: at least this share of a bird's pixels has partial coverage.
/// Measured on lavapipe (near, middle, far): the wing before `edge_coverage` 0.11, 0.10, 0.00, and
/// with it 0.26, 0.38, 0.67.
const BIRD_PARTIAL_SHARE: f32 = 0.2;

/// The binary's one shared device, one test at a time: per-test devices on parallel threads hung
/// the NVIDIA driver (RTX 5090 GPU pre-gate, ADR-7483).
/// It is also the Intel Arc fix of ADR-7557: a test opening and dropping its own device destroyed
/// one while other test threads named objects, the Vulkan loader list race of ADR-2113
/// (`a_low_sun_shades_the_motes_behind_a_trunk` died in `loader_get_icd_and_device`). Every test
/// still builds its own renderer, scene and layers on it.
fn device() -> SerialGpu {
    match serial_device() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn quad(corners: [[f32; 3]; 4], n: [f32; 3]) -> (Vec<LabVertex>, Vec<u32>) {
    (
        corners
            .into_iter()
            .zip([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(p, n, uv))
            .collect(),
        vec![0, 2, 1, 0, 3, 2],
    )
}

/// A dark forest floor and a bright backdrop far behind it (a hazy sky between the trees).
fn scene() -> LabScene {
    let (floor_v, floor_i) = quad(
        [
            [-80.0, 0.0, -80.0],
            [80.0, 0.0, -80.0],
            [80.0, 0.0, 80.0],
            [-80.0, 0.0, 80.0],
        ],
        [0.0, 1.0, 0.0],
    );
    let (wall_v, wall_i) = quad(
        [
            [-150.0, -5.0, -90.0],
            [150.0, -5.0, -90.0],
            [150.0, 120.0, -90.0],
            [-150.0, 120.0, -90.0],
        ],
        [0.0, 0.0, 1.0],
    );
    let floor = LabMaterial {
        base_color: [0.06, 0.07, 0.04, 1.0],
        roughness: 0.9,
        ..LabMaterial::default()
    };
    let wall = LabMaterial {
        base_color: [0.8, 0.82, 0.85, 1.0],
        roughness: 1.0,
        ..LabMaterial::default()
    };
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 1.7, 6.0],
        target: [0.0, 4.0, -20.0],
        fov_y: 1.0,
        near: 0.1,
        far: 300.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 6_000.0,
        double_sided: true,
        post: None,
        // A low sun ahead of the eye: motes and gnat wings glint against it.
        directional: DirectionalLightUniform::new([0.0, -0.35, 1.0], [1.0, 0.92, 0.8], 30_000.0),
        local_lights: Vec::new(),
        materials: vec![floor, wall],
        meshes: vec![
            LabMesh {
                vertices: floor_v,
                indices: floor_i,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: wall_v,
                indices: wall_i,
                material: 1,
                ..LabMesh::unskinned()
            },
        ],
    }
}

/// The full High budget of the kinds in `keep` at the largest density scale (4), so a 480 x 270
/// frame holds enough creatures to measure.
fn counts(keep: &[&str]) -> AmbientCounts {
    let b = AmbientBudget::for_quality(QualityTier::High);
    let pick = |name: &str, n: u32| if keep.contains(&name) { 4.0 * n as f32 } else { 0.0 };
    AmbientCounts {
        motes: pick("motes", b.motes),
        swarm_members: pick("swarms", b.swarm_members),
        butterflies: pick("butterflies", b.butterflies),
        fireflies: pick("fireflies", b.fireflies),
        leaves: pick("leaves", b.leaves),
        birds: 0.0,
    }
}

fn state(keep: &[&str], season: Season, clock_s: f64) -> AmbientState {
    AmbientState {
        integrator: AmbientIntegrator {
            clock_s,
            ..AmbientIntegrator::default()
        },
        counts: counts(keep),
        season,
        seed: 3,
        active: true,
        quality: QualityTier::High,
        ..AmbientState::default()
    }
}

/// A flock ahead of the eye, flown for a few seconds, written into `state` in world space.
fn with_birds(mut state: AmbientState) -> AmbientState {
    let config = AmbientBirds {
        count: 16,
        radius_m: 10.0,
        height_m: 10.0,
        wingspan_m: 0.4,
        seed: 4,
        ..AmbientBirds::flock()
    };
    let mut sim = BirdFlockSim::new(Vec3::new(0.0, 0.0, -30.0), &config);
    for _ in 0..240 {
        sim.step(1.0 / 60.0);
    }
    let written = sim.write_instances([0.0; 3], 16.0, &mut state.birds);
    assert!(written == 16 && written <= MAX_BIRDS);
    state.bird_count = written as u32;
    state.counts.birds = written as f32;
    state
}

fn renderer(device: &RenderDevice, ambient: Option<AmbientState>) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    let gpu = match VfxGpu::new(device, VfxProfile::default()) {
        Ok(gpu) => gpu,
        Err(err) => panic!("vfx: {err:?}"),
    };
    let gpu = Arc::new(Mutex::new(gpu));
    match ambient {
        Some(state) => {
            let handle = AmbientLifeHandle::default();
            if let Ok(mut slot) = handle.state.lock() {
                *slot = state;
            }
            if let Ok(mut slot) = handle.gpu.lock() {
                *slot = AmbientLifeGpu::new(device, &VfxProfile::default()).ok();
            }
            renderer.set_translucent_layers(vec![translucent_layer_with_ambient_life(
                gpu,
                None,
                handle,
                device.queue.clone(),
            )]);
        }
        None => renderer.set_translucent_layers(vec![regia_vfx::translucent_layer(gpu)]),
    }
    renderer
}

fn render(renderer: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene) -> Vec<u8> {
    match renderer.render_offscreen(device, &lab.as_frame_scene_with_quality(FrameQuality::LAB)) {
        Ok(frame) => frame.pixels,
        Err(err) => panic!("frame: {err}"),
    }
}

/// Writes `pixels` as a PNG under `REGIA_AMBIENT_SHEET_DIR` (for a look by eye), if it is set.
fn sheet(name: &str, pixels: &[u8]) {
    let Some(dir) = std::env::var_os("REGIA_AMBIENT_SHEET_DIR") else {
        return;
    };
    let path = std::path::Path::new(&dir).join(format!("{name}.png"));
    let file = std::fs::File::create(&path).expect("sheet file");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .expect("header")
        .write_image_data(pixels)
        .expect("pixels");
}

/// `(x, y, before, after)` of every pixel whose largest channel moved by more than 1 of 255.
fn changed(a: &[u8], b: &[u8]) -> Vec<(u32, u32, [u8; 3], [u8; 3])> {
    let mut out = Vec::new();
    for y in 0..H {
        for x in 0..W {
            let at = ((y * W + x) * 4) as usize;
            if (0..3).any(|c| a[at + c].abs_diff(b[at + c]) > 1) {
                out.push((x, y, [a[at], a[at + 1], a[at + 2]], [b[at], b[at + 1], b[at + 2]]));
            }
        }
    }
    out
}

#[test]
fn off_draws_the_frame_a_host_without_it_draws() {
    let device = device();
    let lab = scene();
    let plain = render(&mut renderer(&device, None), &device, &lab);
    let off = render(
        &mut renderer(&device, Some(AmbientState::default())),
        &device,
        &lab,
    );
    assert!(
        plain == off,
        "an inactive ambient layer changed {} pixels",
        changed(&plain, &off).len()
    );
}

#[test]
fn a_summer_afternoon_is_alive_and_draws_the_same_bits_twice() {
    let device = device();
    let lab = scene();
    let plain = render(&mut renderer(&device, None), &device, &lab);
    let alive = state(
        &["motes", "swarms", "butterflies", "leaves"],
        Season::Summer,
        12.0,
    );
    let a = render(&mut renderer(&device, Some(alive.clone())), &device, &lab);
    let b = render(&mut renderer(&device, Some(alive)), &device, &lab);
    sheet("summer", &a);
    assert!(a == b, "the same state drew different frames");
    let moved = changed(&plain, &a);
    assert!(moved.len() > 150, "{} pixels changed", moved.len());
}

#[test]
fn autumn_leaves_fall_in_autumn_colors() {
    let device = device();
    let lab = scene();
    let plain = render(&mut renderer(&device, None), &device, &lab);
    let autumn = render(
        &mut renderer(&device, Some(state(&["leaves"], Season::Autumn, 5.0))),
        &device,
        &lab,
    );
    sheet("autumn_leaves", &autumn);
    let moved = changed(&plain, &autumn);
    assert!(moved.len() > 100, "{} leaf pixels", moved.len());
    // Over the bright backdrop a leaf is warm: more red than blue.
    let over_wall: Vec<_> = moved.iter().filter(|(.., before, _)| before[2] > 120).collect();
    let warm = over_wall.iter().filter(|(.., after)| after[0] > after[2]).count();
    assert!(
        !over_wall.is_empty() && warm * 10 >= over_wall.len() * 7,
        "{warm} of {} warm",
        over_wall.len()
    );
}

#[test]
fn birds_are_dark_shapes_where_the_flock_flies() {
    let device = device();
    let lab = scene();
    let plain = render(&mut renderer(&device, None), &device, &lab);
    let birds = render(
        &mut renderer(&device, Some(with_birds(state(&[], Season::Summer, 1.0)))),
        &device,
        &lab,
    );
    sheet("birds", &birds);
    let moved = changed(&plain, &birds);
    assert!(moved.len() > 10, "{} bird pixels", moved.len());
    let darker = moved
        .iter()
        .filter(|(.., before, after)| u32::from(after[1]) < u32::from(before[1]))
        .count();
    assert!(
        darker * 10 >= moved.len() * 9,
        "{darker} of {} darker",
        moved.len()
    );
    // The flock flies 10 m up, 30 m out: above the horizon line of the eye.
    let high = moved.iter().filter(|(_, y, ..)| *y < H / 2).count();
    assert!(high * 10 >= moved.len() * 9, "{high} of {} high", moved.len());
}

/// A bird at about the eye's height has to read from every side. A gliding bird's wings lie almost
/// in the plane of the view, so wings alone (the first shape) drew nothing at all: in the real
/// forest a flock at its own height was a few thin dashes. One 40 cm bird 24 m out, dark on the
/// bright backdrop, at 1280 x 720 (about 27 pixels a metre).
#[test]
fn a_bird_reads_from_its_own_height() {
    let device = device();
    let lab = LabScene {
        width: 1280,
        height: 720,
        target: [0.0, 1.7, -20.0],
        ..scene()
    };
    let plain = render(&mut renderer(&device, None), &device, &lab);
    // World positions: the eye is at (0, 1.7, 6).
    let flat = 0.0;
    let mut failed = Vec::new();
    for (name, position, heading, wing, least) in [
        // Wings in the view's plane. The body is about 7 by 2 pixels.
        ("flat, flying across", [0.0, 1.7, -18.0], [1.0, 0.0, 0.0], flat, 6),
        // Wings straight across the view, seen on their edge: about 11 pixels wide.
        (
            "flat, flying at the eye",
            [0.0, 1.7, -18.0],
            [0.0, 0.0, 1.0],
            flat,
            8,
        ),
        ("flat, flying away", [0.0, 1.7, -18.0], [0.0, 0.0, -1.0], flat, 8),
        (
            "wings up, flying across",
            [0.0, 1.7, -18.0],
            [1.0, 0.0, 0.0],
            0.6,
            8,
        ),
        // Seen from below: the wings show their undersides.
        (
            "flat, above, flying across",
            [0.0, 9.7, -18.0],
            [1.0, 0.0, 0.0],
            flat,
            8,
        ),
        (
            "flat, above, flying at the eye",
            [0.0, 9.7, -18.0],
            [0.0, 0.0, 1.0],
            flat,
            10,
        ),
    ] {
        let mut ambient = state(&[], Season::Summer, 1.0);
        ambient.birds[0] = BirdInstance {
            position_size: [position[0], position[1], position[2], 0.4],
            heading_wing: [heading[0], heading[1], heading[2], wing],
            color_alpha: [0.035, 0.032, 0.03, 1.0],
        };
        ambient.bird_count = 1;
        ambient.counts.birds = 1.0;
        let frame = render(&mut renderer(&device, Some(ambient)), &device, &lab);
        let darker = plain
            .as_chunks::<4>()
            .0
            .iter()
            .zip(frame.as_chunks::<4>().0)
            .filter(|(a, b)| b[1].saturating_add(12) < a[1])
            .count();
        let moved: Vec<_> = plain
            .as_chunks::<4>()
            .0
            .iter()
            .zip(frame.as_chunks::<4>().0)
            .enumerate()
            .filter(|(_, (a, b))| (0..3).any(|c| a[c].abs_diff(b[c]) > 2))
            .map(|(at, (a, b))| (at % 1280, at / 1280, a[1], b[1]))
            .collect();
        println!(
            "{name}: {darker} pixels darker by more than 12 of 255; {} moved (x, y, before, after): {:?}",
            moved.len(),
            &moved[..moved.len().min(24)]
        );
        if darker < least {
            failed.push(format!("{name}: {darker} pixels, at least {least} expected"));
        }
    }
    assert!(failed.is_empty(), "{failed:?}");
}

/// The layer draws after the temporal resolve, so a bird's edge is only as soft as its fragment makes
/// it: most of a bird that spans a few pixels must not be either full coverage or none. Measured
/// as the share of its pixels with partial coverage, against the darkest pixel of the bird.
#[test]
fn a_birds_edge_is_a_pixel_wide() {
    let device = device();
    let lab = LabScene {
        width: 1280,
        height: 720,
        target: [0.0, 1.7, -20.0],
        ..scene()
    };
    let plain = render(&mut renderer(&device, None), &device, &lab);
    let mut failed = Vec::new();
    for (name, position, wing) in [
        ("near", [0.3, 1.9, -6.0], 0.6),
        ("middle", [0.1, 2.1, -18.0], 0.6),
        ("far", [0.2, 3.6, -40.0], 0.6),
    ] {
        let mut ambient = state(&[], Season::Summer, 1.0);
        ambient.birds[0] = BirdInstance {
            position_size: [position[0], position[1], position[2], 0.7],
            heading_wing: [1.0, 0.0, 0.0, wing],
            color_alpha: [0.035, 0.032, 0.03, 1.0],
        };
        ambient.bird_count = 1;
        ambient.counts.birds = 1.0;
        let frame = render(&mut renderer(&device, Some(ambient)), &device, &lab);
        let drops: Vec<f32> = plain
            .as_chunks::<4>()
            .0
            .iter()
            .zip(frame.as_chunks::<4>().0)
            .map(|(a, b)| f32::from(a[1]) - f32::from(b[1]))
            .filter(|drop| *drop > 2.0)
            .collect();
        let full = drops.iter().copied().fold(0.0_f32, f32::max);
        let partial = drops.iter().filter(|drop| **drop < 0.8 * full).count();
        let share = partial as f32 / drops.len().max(1) as f32;
        println!(
            "{name}: {} pixels, deepest drop {full} of 255, {partial} partial, share {share:.2}",
            drops.len()
        );
        if drops.is_empty() || share < BIRD_PARTIAL_SHARE {
            failed.push(format!("{name}: share {share:.2} of {} pixels", drops.len()));
        }
    }
    assert!(failed.is_empty(), "{failed:?}");
}

#[test]
fn fireflies_only_add_light() {
    let device = device();
    let mut lab = scene();
    // Dusk: a dim sun, a dim sky.
    lab.directional = DirectionalLightUniform::new([0.0, -0.05, 1.0], [1.0, 0.6, 0.4], 800.0);
    lab.ambient_lux = 200.0;
    let plain = render(&mut renderer(&device, None), &device, &lab);
    let mut lit = 0;
    for clock in [0.0, 1.0, 2.0, 3.0] {
        let glow = render(
            &mut renderer(&device, Some(state(&["fireflies"], Season::Summer, clock))),
            &device,
            &lab,
        );
        if clock == 0.0 {
            sheet("fireflies", &glow);
        }
        let moved = changed(&plain, &glow);
        for (x, y, before, after) in &moved {
            assert!(
                (0..3).all(|c| after[c] >= before[c]),
                "({x}, {y}) went darker: {before:?} -> {after:?}"
            );
        }
        lit += moved.len();
    }
    assert!(lit > 4, "{lit} firefly pixels over four moments");
}

/// Under a low roof, with the sun straight overhead: the eye stands 0.5 m under the slab, 0.2 m
/// from its edge, and looks up, so the left of the frame is under the roof and the right sees open
/// sky through air close enough for a millimetre mote to cover a good part of a pixel.
fn roofed() -> LabScene {
    let mut lab = scene();
    let (roof_v, roof_i) = quad(
        [
            [-60.0, 1.5, -60.0],
            [-0.8, 1.5, -60.0],
            [-0.8, 1.5, 60.0],
            [-60.0, 1.5, 60.0],
        ],
        [0.0, -1.0, 0.0],
    );
    lab.meshes.truncate(1);
    lab.meshes.push(LabMesh {
        vertices: roof_v,
        indices: roof_i,
        ..LabMesh::unskinned()
    });
    lab.eye = [-1.0, 1.0, 0.0];
    lab.target = [-1.0, 10.0, -3.0];
    lab.directional = DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0, 0.95, 0.85], 60_000.0);
    lab
}

/// Pixels of columns `x0..x1` whose largest channel rose by more than 40 of 255: glints, not the
/// motes' faint sky light.
fn glints(a: &[u8], b: &[u8], (x0, x1): (u32, u32)) -> usize {
    let mut count = 0;
    for y in 0..H {
        for x in x0..x1 {
            let at = ((y * W + x) * 4) as usize;
            count += usize::from((0..3).any(|c| b[at + c] > a[at + c].saturating_add(40)));
        }
    }
    count
}

/// The motes ask for the occlusion view along the sun and glint only where it reaches them: in the
/// open half, not under the roof. Without the view they glint under the roof too.
#[test]
fn motes_glint_in_the_sun_and_not_in_the_shade() {
    let device = device();
    let lab = roofed();
    let plain = render(&mut renderer(&device, None), &device, &lab);
    let shafts = |wants_sun_view: bool| {
        let mut state = state(&["motes"], Season::Summer, 2.0);
        state.wants_sun_view = wants_sun_view;
        let mut r = renderer(&device, Some(state));
        let pixels = render(&mut r, &device, &lab);
        (pixels, r.sun_occlusion())
    };
    let (lit, info) = shafts(true);
    sheet("motes_shade", &lit);
    let info = info.expect("the motes asked for the occlusion view");
    assert_eq!(info.tilt_deg, 0.0, "the sun is overhead");
    let open = glints(&plain, &lit, (W / 2 + 60, W));
    let covered = glints(&plain, &lit, (0, W / 2 - 40));
    assert!(open > 4, "glints in the sun: {open}");
    assert!(
        covered * 3 <= open,
        "glints in the shade: {covered} (open {open})"
    );
    let (unshaded, info) = shafts(false);
    sheet("motes_noview", &unshaded);
    assert!(info.is_none(), "no view asked for without a sun view");
    let covered_without = glints(&plain, &unshaded, (0, W / 2 - 40));
    let open_without = glints(&plain, &unshaded, (W / 2 + 60, W));
    assert!(
        covered_without >= covered,
        "the view only takes glints away: {covered_without} vs {covered}"
    );
    assert!(
        open_without >= open,
        "the open air keeps its glints: {open_without} vs {open}"
    );
}

/// Pixels of an RGBA frame where a channel rose by more than `levels` against `plain`.
fn brightened(plain: &[u8], frame: &[u8], levels: u8) -> usize {
    plain
        .as_chunks::<4>()
        .0
        .iter()
        .zip(frame.as_chunks::<4>().0)
        .filter(|(a, b)| (0..3).any(|c| b[c] > a[c].saturating_add(levels)))
        .count()
}

/// The tier's real budget has to read on a film-sized frame, not just the 4x density the small
/// frames here use to measure anything. At 1440p in a real forest the first motes, true to a
/// millimetre flake's size, changed 205 of 3.7 million pixels by more than 2 of 255: nobody could
/// see them, while these tests all passed. Looking into the sun and across it.
#[test]
fn the_real_budget_reads_at_film_scale() {
    let device = device();
    let budget = AmbientBudget::for_quality(QualityTier::High);
    let motes = AmbientState {
        counts: AmbientCounts {
            motes: budget.motes as f32,
            ..AmbientCounts::default()
        },
        wants_sun_view: false,
        ..state(&[], Season::Summer, 12.0)
    };
    for (name, target, least) in [
        // The millimetre flakes reached 208 and 65 here.
        ("into the sun", [0.0, 4.0, -20.0], 600),
        ("across the sun", [26.0, 2.0, 6.0], 600),
    ] {
        let lab = LabScene {
            width: 1280,
            height: 720,
            target,
            ..scene()
        };
        let plain = render(&mut renderer(&device, None), &device, &lab);
        let frame = render(&mut renderer(&device, Some(motes.clone())), &device, &lab);
        let seen = brightened(&plain, &frame, 12);
        println!("{name}: {seen} pixels rose by more than 12 of 255");
        assert!(seen >= least, "{name}: {seen} pixels, at least {least} expected");
    }
}

/// A swarm has to read too. Dark specks over a dark forest did not: with 3 swarms' worth drawn in
/// all directions, insects-only frames of the real forest at 1080p showed butterflies and no swarm.
/// The real High budget, 1280 x 720.
#[test]
fn swarms_read_at_film_scale() {
    let device = device();
    let budget = AmbientBudget::for_quality(QualityTier::High);
    let swarms = AmbientState {
        counts: AmbientCounts {
            swarm_members: budget.swarm_members as f32,
            ..AmbientCounts::default()
        },
        wants_sun_view: false,
        ..state(&[], Season::Summer, 12.0)
    };
    for (name, target, least) in [
        ("into the sun", [0.0, 4.0, -20.0], 60),
        ("across the sun", [26.0, 2.0, 6.0], 60),
    ] {
        let lab = LabScene {
            width: 1280,
            height: 720,
            target,
            ..scene()
        };
        let plain = render(&mut renderer(&device, None), &device, &lab);
        let frame = render(&mut renderer(&device, Some(swarms.clone())), &device, &lab);
        let seen = brightened(&plain, &frame, 12);
        println!("{name}: {seen} pixels rose by more than 12 of 255");
        assert!(seen >= least, "{name}: {seen} pixels, at least {least} expected");
    }
}

/// Up close, a butterfly is a butterfly: on the real forest at 1440p, one 40 pixels across read as
/// two flat blue eggs, its only marking a thin outline. The test aims a narrow view at the butterfly
/// seen most from above with its wings most open (found through the CPU mirror of the shader), fills
/// the frame with it, and asks for dark margins and pale spots on the wings, and for wing tips that
/// reach out past the blob two round wings make. Only the tips separate the old wing from this one
/// (reach 1.455 against 1.88 on this frame): the old wing had a dark outline and a pale spot too, so
/// the markings' two limits guard against losing them and do not prove the look.
#[test]
fn a_butterfly_up_close_has_markings_and_wing_tips() {
    let device = device();
    let base = scene();
    let steepness = |position: [f32; 3]| -position[1] / position[0].hypot(position[2]).max(0.1);
    let mut best = (f32::MIN, 0.0, [0.0_f32; 3], 0.0_f32);
    for clock in 0..120 {
        let clock_s = f64::from(clock) * 0.5;
        let frame = state(&["butterflies"], Season::Summer, clock_s).frame_at(base.eye.map(f64::from));
        for index in 0..64 {
            let sample = butterfly_at(index, &frame);
            if sample.alpha >= 1.0 {
                let open = butterfly_wing_angle(index, &frame);
                let score = steepness(sample.position_m) - open;
                if score > best.0 {
                    best = (score, clock_s, sample.position_m, sample.size_m);
                }
            }
        }
    }
    let (_, clock_s, position, size_m) = best;
    let distance = (position[0] * position[0] + position[1] * position[1] + position[2] * position[2]).sqrt();
    let lab = LabScene {
        target: [
            base.eye[0] + position[0],
            base.eye[1] + position[1],
            base.eye[2] + position[2],
        ],
        // Both wings fill about 70 % of the frame's height.
        fov_y: 2.0 * (size_m / 0.7 / distance).atan(),
        ..scene()
    };
    let plain = render(&mut renderer(&device, None), &device, &lab);
    let lit = state(&["butterflies"], Season::Summer, clock_s);
    let frame = render(&mut renderer(&device, Some(lit)), &device, &lab);
    sheet("butterfly-close", &frame);
    let wing = changed(&plain, &frame);
    assert!(wing.len() > 2000, "too few pixels for a look: {}", wing.len());
    let luma = |c: [u8; 3]| 0.2126 * f32::from(c[0]) + 0.7152 * f32::from(c[1]) + 0.0722 * f32::from(c[2]);
    let mut lumas: Vec<f32> = wing.iter().map(|p| luma(p.3)).collect();
    lumas.sort_by(f32::total_cmp);
    let median = lumas[lumas.len() / 2];
    let share =
        |keep: &dyn Fn(f32) -> bool| lumas.iter().filter(|l| keep(**l)).count() as f32 / lumas.len() as f32;
    let dark = share(&|l| l < median * 0.7);
    let pale = share(&|l| l > median * 1.1);
    let n = wing.len() as f32;
    let (cx, cy) = wing
        .iter()
        .fold((0.0, 0.0), |(x, y), p| (x + p.0 as f32 / n, y + p.1 as f32 / n));
    let reach = wing
        .iter()
        .map(|p| (p.0 as f32 - cx).hypot(p.1 as f32 - cy))
        .fold(0.0, f32::max);
    let tips = reach / (n / std::f32::consts::PI).sqrt();
    println!(
        "butterfly {} pixels at clock {clock_s}: dark {dark:.3}, pale {pale:.3}, tips {tips:.3}, median {median:.1}",
        wing.len()
    );
    assert!(dark >= 0.15, "no dark markings: {dark}");
    assert!(pale >= 0.01, "no pale markings: {pale}");
    assert!(tips >= 1.6, "no wing tips: {tips}");
}

/// Elevation of the golden-hour sun below, whose light travels toward +Z.
const LOW_SUN_DEG: f32 = 15.0;

/// A golden hour: a tall wall up-sun of the eye, whose edge runs just right of it, and the eye
/// looking into the sun past that edge. Motes left of the center hang in the wall's shadow, motes
/// right of it in the open sun. Nothing but the floor and the wall to glint against.
fn walled() -> LabScene {
    let mut lab = scene();
    let (wall_v, wall_i) = quad(
        [
            [-60.0, 0.0, -8.0],
            [-0.8, 0.0, -8.0],
            [-0.8, 60.0, -8.0],
            [-60.0, 60.0, -8.0],
        ],
        [0.0, 0.0, 1.0],
    );
    lab.meshes.truncate(1);
    lab.meshes.push(LabMesh {
        vertices: wall_v,
        indices: wall_i,
        ..LabMesh::unskinned()
    });
    let (sin_e, cos_e) = LOW_SUN_DEG.to_radians().sin_cos();
    lab.eye = [-1.0, 1.7, 0.0];
    lab.target = [-1.0, 1.7 + 3.0 * sin_e, -3.0 * cos_e];
    lab.directional = DirectionalLightUniform::new([0.0, -sin_e, cos_e], [1.0, 0.85, 0.65], 60_000.0);
    lab
}

/// Past the 45° the rain's view stops at: under a 15° sun the view tilts with it, and the wall up-sun
/// takes the glints from the air in its shadow.
#[test]
fn a_low_sun_shades_the_motes_behind_a_trunk() {
    let device = device();
    let lab = walled();
    let plain = render(&mut renderer(&device, None), &device, &lab);
    let shafts = |wants_sun_view: bool| {
        let mut state = state(&["motes"], Season::Summer, 2.0);
        state.wants_sun_view = wants_sun_view;
        let mut r = renderer(&device, Some(state));
        let pixels = render(&mut r, &device, &lab);
        (pixels, r.sun_occlusion())
    };
    let (lit, info) = shafts(true);
    sheet("motes_low_sun", &lit);
    let info = info.expect("the motes asked for the occlusion view");
    assert_eq!(
        info.tilt_deg,
        90.0 - LOW_SUN_DEG,
        "the frame's own sun, on its step"
    );
    let (unshaded, _) = shafts(false);
    sheet("motes_low_sun_noview", &unshaded);
    let side = |pixels: &[u8]| {
        (
            glints(&plain, pixels, (W / 2 + 40, W)),
            glints(&plain, pixels, (0, W / 2 - 40)),
        )
    };
    let (open, covered) = side(&lit);
    let (open_without, covered_without) = side(&unshaded);
    println!(
        "low sun: open {open}, covered {covered}; no view: open {open_without}, covered {covered_without}"
    );
    assert!(open > 4, "glints in the sun: {open}");
    assert!(
        covered * 3 <= open,
        "glints in the shadow: {covered} (open {open})"
    );
    assert!(
        covered_without > covered * 3,
        "without the view the shadow glints: {covered_without} vs {covered}"
    );
    assert!(
        open_without >= open,
        "the open air keeps its glints: {open_without} vs {open}"
    );
}

const WET: SurfaceWeatherFrame = SurfaceWeatherFrame {
    wetness: 0.8,
    puddles: 0.4,
    snow_cover: 0.0,
    ripple: 0.0,
    clock_s: 1.0,
};

/// A layer that draws nothing and wishes for the sun view.
fn only_wishing() -> AmbientState {
    AmbientState {
        wants_sun_view: true,
        ..state(&[], Season::Summer, 2.0)
    }
}

/// On a dry frame the sun view changes no pixel but the motes': the wish alone draws the frame a
/// host without the layer draws.
#[test]
fn the_sun_view_alone_changes_no_pixel() {
    let device = device();
    let lab = walled();
    let plain = render(&mut renderer(&device, None), &device, &lab);
    let mut wishing = renderer(&device, Some(only_wishing()));
    let drawn = render(&mut wishing, &device, &lab);
    assert!(wishing.sun_occlusion().is_some());
    assert!(
        wishing.precipitation_occlusion().is_none(),
        "the rain's accessor does not report it"
    );
    assert!(plain == drawn, "{} pixels moved", changed(&plain, &drawn).len());
}

/// The surface shading reads the tile on a wet frame, so the sun must not take it there. With the
/// tier's view the tile stays straight down; with no view it stays absent. Either way the wish
/// moves no pixel.
#[test]
fn a_wet_surface_keeps_its_view_from_the_sun() {
    let device = device();
    let lab = walled();
    let budget = PrecipBudget::for_quality(QualityTier::High).occlusion;
    let frame = |ambient: Option<AmbientState>, budget: Option<(u32, f32)>| {
        let mut r = renderer(&device, ambient);
        r.set_surface_weather(WET);
        r.set_precipitation_occlusion_budget(budget, 16_384);
        let pixels = render(&mut r, &device, &lab);
        (pixels, r.precipitation_occlusion())
    };
    let (plain, info) = frame(None, budget);
    info.expect("the wet surface asks for the view");
    let (wishing, info) = frame(Some(only_wishing()), budget);
    let info = info.expect("the wet surface keeps the view");
    assert_eq!(info.tilt_deg, 0.0, "straight down, not along the sun");
    assert!(
        plain == wishing,
        "{} pixels moved",
        changed(&plain, &wishing).len()
    );
    let (plain, info) = frame(None, None);
    assert_eq!(info, None);
    let (wishing, info) = frame(Some(only_wishing()), None);
    assert_eq!(info, None, "open sky on the surface, as without the wish");
    assert!(
        plain == wishing,
        "{} pixels moved",
        changed(&plain, &wishing).len()
    );
    // The motes do not read the straight-down view as the sun's: they glint as with no view at all.
    let motes = |wants_sun_view: bool| AmbientState {
        wants_sun_view,
        ..state(&["motes"], Season::Summer, 2.0)
    };
    let (wish, _) = frame(Some(motes(true)), budget);
    let (no_wish, _) = frame(Some(motes(false)), budget);
    assert!(wish == no_wish, "{} pixels moved", changed(&wish, &no_wish).len());
}

/// Rain falling with `occlusion` as given, for a layer that also holds ambient life.
fn rain(occlusion: bool) -> PrecipState {
    let config = WeatherConfig::default();
    let settings = PrecipitationSettings {
        enabled: true,
        occlusion,
        ..PrecipitationSettings::from_config(&config)
    };
    let rates = WeatherState::STORM.rates(&config);
    let budget = PrecipBudget::for_quality(QualityTier::High);
    let mut integrator = PrecipIntegrator::default();
    for _ in 0..10 {
        integrator.step(&rates, &config, [0.0, 0.0], &budget, 1.0 / 60.0);
    }
    PrecipState {
        integrator,
        rates,
        config,
        settings,
        quality: QualityTier::High,
        active: true,
        clock_s: 1.0,
        ..PrecipState::default()
    }
}

fn renderer_with_rain(device: &RenderDevice, precip: PrecipState, ambient: AmbientState) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    let gpu = match VfxGpu::new(device, VfxProfile::default()) {
        Ok(gpu) => gpu,
        Err(err) => panic!("vfx: {err:?}"),
    };
    let rain = PrecipitationHandle::default();
    if let Ok(mut slot) = rain.state.lock() {
        *slot = precip;
    }
    if let Ok(mut slot) = rain.gpu.lock() {
        *slot = PrecipitationGpu::new(device, &VfxProfile::default()).ok();
    }
    let life = AmbientLifeHandle::default();
    if let Ok(mut slot) = life.state.lock() {
        *slot = ambient;
    }
    if let Ok(mut slot) = life.gpu.lock() {
        *slot = AmbientLifeGpu::new(device, &VfxProfile::default()).ok();
    }
    renderer.set_translucent_layers(vec![translucent_layer_with_ambient_life(
        Arc::new(Mutex::new(gpu)),
        Some(rain),
        life,
        device.queue.clone(),
    )]);
    renderer
}

/// Rain keeps the tile while it falls, and rain with its occlusion off is not handed the sun's view,
/// which would hide drops in the sun's shadow.
#[test]
fn rain_never_reads_the_sun_view() {
    let device = device();
    let lab = walled();
    let budget = PrecipBudget::for_quality(QualityTier::High).occlusion;
    let quiet = AmbientState {
        wants_sun_view: false,
        ..only_wishing()
    };

    let mut keeps = renderer_with_rain(&device, rain(true), only_wishing());
    keeps.set_precipitation_occlusion_budget(budget, 16_384);
    let wishing = render(&mut keeps, &device, &lab);
    let info = keeps.precipitation_occlusion().expect("the rain's view");
    assert!(keeps.sun_occlusion().is_none(), "{info:?}");
    assert_eq!(info.tilt_deg, 0.0, "no wind");
    let mut alone = renderer_with_rain(&device, rain(true), quiet.clone());
    alone.set_precipitation_occlusion_budget(budget, 16_384);
    let without = render(&mut alone, &device, &lab);
    assert!(
        wishing == without,
        "{} pixels moved",
        changed(&wishing, &without).len()
    );

    let mut unbound = renderer_with_rain(&device, rain(false), only_wishing());
    let wishing = render(&mut unbound, &device, &lab);
    assert!(unbound.sun_occlusion().is_some(), "nothing else wants the tile");
    let without = render(
        &mut renderer_with_rain(&device, rain(false), quiet),
        &device,
        &lab,
    );
    assert!(
        wishing == without,
        "rain read the sun's view: {} pixels moved",
        changed(&wishing, &without).len()
    );
}

/// Frame cost of each kind at the High budget and density 1 at 1440p, against the same frame without
/// the layer, frames interleaved so the machine's load falls on all alike. On lavapipe the numbers
/// are an upper bound on the work, not a card's time:
/// `cargo test -p regia_vfx --test ambient_frame bench -- --ignored --nocapture`.
#[test]
#[ignore = "prints numbers"]
fn bench_ambient_life_frame_cost() {
    const ROUNDS: usize = 9;
    let device = device();
    let mut lab = scene();
    lab.width = 2560;
    lab.height = 1440;
    let budget = AmbientBudget::for_quality(QualityTier::High);
    let of = |counts: AmbientCounts, wants_sun_view: bool| AmbientState {
        counts,
        wants_sun_view,
        ..state(&[], Season::Summer, 12.0)
    };
    let none = AmbientCounts::default();
    let everything = AmbientCounts {
        motes: budget.motes as f32,
        swarm_members: budget.swarm_members as f32,
        butterflies: budget.butterflies as f32,
        fireflies: budget.fireflies as f32,
        leaves: budget.leaves as f32,
        birds: 0.0,
    };
    let kinds = [
        ("off", None),
        ("wish only (tile)", Some(of(none, true))),
        (
            "motes",
            Some(of(
                AmbientCounts {
                    motes: everything.motes,
                    ..none
                },
                true,
            )),
        ),
        (
            "swarms",
            Some(of(
                AmbientCounts {
                    swarm_members: everything.swarm_members,
                    ..none
                },
                true,
            )),
        ),
        (
            "butterflies",
            Some(of(
                AmbientCounts {
                    butterflies: everything.butterflies,
                    ..none
                },
                false,
            )),
        ),
        (
            "fireflies",
            Some(of(
                AmbientCounts {
                    fireflies: everything.fireflies,
                    ..none
                },
                false,
            )),
        ),
        (
            "leaves",
            Some(of(
                AmbientCounts {
                    leaves: everything.leaves,
                    ..none
                },
                false,
            )),
        ),
        ("birds", Some(with_birds(of(none, false)))),
        ("all", Some(with_birds(of(everything, true)))),
    ];
    let mut runs: Vec<_> = kinds
        .into_iter()
        .map(|(label, ambient)| {
            let mut r = renderer(&device, ambient);
            render(&mut r, &device, &lab);
            (label, r, Vec::with_capacity(ROUNDS))
        })
        .collect();
    for _ in 0..ROUNDS {
        for (_, r, times) in &mut runs {
            let started = std::time::Instant::now();
            render(r, &device, &lab);
            times.push(started.elapsed().as_secs_f64() * 1000.0);
        }
    }
    let median = |times: &mut Vec<f64>| {
        times.sort_by(f64::total_cmp);
        times[ROUNDS / 2]
    };
    let mut off = 0.0;
    for (label, _, times) in &mut runs {
        let ms = median(times);
        if *label == "off" {
            off = ms;
        }
        println!("{label}: {ms:.2} ms a frame, {:+.2} ms", ms - off);
    }
}
