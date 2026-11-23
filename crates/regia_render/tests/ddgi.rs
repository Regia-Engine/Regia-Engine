// SPDX-License-Identifier: MPL-2.0
//! GPU irradiance probes (DDGI, spec 27 § 4.2 stage 2a) on the runtime frame path.
//!
//! The failure mode of probe GI is light leaking through thin walls: a closed room under
//! the sun must stay dark inside (spec 27 § 2.2: interior ≤ 0.5 % of the exterior ground;
//! the lane's acceptance asks ≤ 1 %). Open ground under an empty sky must read the sky's
//! own irradiance (the probes integrate the sky, nothing more), and a held camera must not
//! flicker once the probes converged.

use regia_core::glam::Mat4;
use regia_render::{
    DdgiSettings, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    LabVertex, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, box_mesh,
};

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip ddgi: {err}");
            None
        }
    }
}

const W: u32 = 160;
const H: u32 = 96;

/// A box of half extents `half` centred at `centre`.
fn block(centre: [f32; 3], half: [f32; 3], material: usize) -> LabMesh {
    let (vertices, indices) = box_mesh(half);
    LabMesh {
        vertices,
        indices,
        material,
        model: Mat4::from_translation(centre.into()).to_cols_array_2d(),
        ..LabMesh::unskinned()
    }
}

fn grey(v: f32) -> LabMaterial {
    LabMaterial {
        base_color: [v, v, v, 1.0],
        roughness: 0.9,
        ..LabMaterial::default()
    }
}

fn quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.render_scale_permille = 1000;
    q
}

/// Probe volumes small enough for lavapipe: two cascades of 12×8×12 at 1 m and 3 m,
/// every probe re-traced every frame, bricks baked on the caller.
fn test_settings() -> DdgiSettings {
    DdgiSettings {
        cascades: 2,
        dims: [12, 8, 12],
        spacing: 1.0,
        update_divisor: 1,
        bake_on_caller: true,
        min_proxy_radius: 0.2,
        ..DdgiSettings::default()
    }
}

fn sun(elevation_deg: f32, lux: f32) -> DirectionalLightUniform {
    let e = elevation_deg.to_radians();
    let az = 0.6f32;
    DirectionalLightUniform::new(
        [-(e.cos() * az.cos()), -e.sin(), -(e.cos() * az.sin())],
        [1.0; 3],
        lux,
    )
}

/// Mean BT.709 luma of a pixel rectangle, decoded to linear light.
fn linear_luma(pixels: &[u8], x0: u32, y0: u32, x1: u32, y1: u32) -> f32 {
    let lin = |c: u8| {
        let s = f32::from(c) / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * W + x) * 4) as usize;
            sum += 0.2126 * lin(pixels[i]) + 0.7152 * lin(pixels[i + 1]) + 0.0722 * lin(pixels[i + 2]);
            n += 1.0;
        }
    }
    sum / n
}

fn scene(eye: [f32; 3], target: [f32; 3], meshes: Vec<LabMesh>, materials: Vec<LabMaterial>) -> LabScene {
    let mut post = PostSettings::golden_deterministic();
    post.exposure_ev = 14.5;
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
        exposure_ev: 14.5,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: Some(post),
        directional: sun(50.0, 100_000.0),
        local_lights: Vec::new(),
        materials,
        meshes,
    }
}

/// Writes `pixels` to `$DDGI_DUMP/<name>.png` when that variable is set (debugging).
fn dump(name: &str, pixels: &[u8]) {
    let Some(dir) = regia_core::brand::env_var_os("DDGI_DUMP") else {
        return;
    };
    let path = std::path::Path::new(&dir).join(format!("{name}.png"));
    let Ok(file) = std::fs::File::create(path) else {
        return;
    };
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    if let Ok(mut writer) = encoder.write_header() {
        let _ = writer.write_image_data(pixels);
    }
}

/// Renders `frames` frames and returns the last one.
fn converge(renderer: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene, frames: u32) -> Vec<u8> {
    converge_with(renderer, device, lab, frames, quality())
}

/// [`converge`] at quality `q`.
fn converge_with(
    renderer: &mut FrameRenderer,
    device: &RenderDevice,
    lab: &LabScene,
    frames: u32,
    q: FrameQuality,
) -> Vec<u8> {
    let mut last = Vec::new();
    for _ in 0..frames {
        let frame = lab.as_frame_scene_with_quality(q);
        last = renderer
            .render_offscreen(device, &frame)
            .unwrap_or_else(|err| panic!("frame: {err}"))
            .pixels;
    }
    last
}

/// Ground slab plus a 6 × 3 × 6 m room with 0.2 m walls and roof, sealed.
fn sealed_room() -> Vec<LabMesh> {
    let t = 0.1;
    vec![
        block([0.0, -0.25, 0.0], [30.0, 0.25, 30.0], 0),
        block([3.0 + t, 1.5, 0.0], [t, 1.5, 3.0 + 2.0 * t], 0),
        block([-3.0 - t, 1.5, 0.0], [t, 1.5, 3.0 + 2.0 * t], 0),
        block([0.0, 1.5, 3.0 + t], [3.0, 1.5, t], 0),
        block([0.0, 1.5, -3.0 - t], [3.0, 1.5, t], 0),
        block([0.0, 3.0 + t, 0.0], [3.0 + 2.0 * t, t, 3.0 + 2.0 * t], 0),
    ]
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn a_sealed_room_under_the_sun_stays_dark_inside() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(test_settings());
    renderer.set_ddgi_enabled(Some(true));
    // Outside: the sunlit ground beside the room.
    let outside = scene([10.0, 2.5, 10.0], [6.0, 0.0, 6.0], sealed_room(), vec![grey(0.6)]);
    let out_px = converge(&mut renderer, &device, &outside, 40);
    assert!(renderer.ddgi_active(), "Desktop High runs the probes");
    let exterior = linear_luma(&out_px, 40, 50, 120, 90);
    // Inside: the floor, lit only by what leaks through the walls and roof.
    let inside = scene([0.0, 1.6, -1.5], [0.0, 0.0, 1.5], sealed_room(), vec![grey(0.6)]);
    let in_px = converge(&mut renderer, &device, &inside, 60);
    if regia_core::brand::env_var_os("DDGI_PROBES").is_some()
        && let Ok(Some(rb)) = renderer.ddgi_read_back(&device)
    {
        eprintln!(
            "instances {} lists {:?} bases {:?}",
            rb.instances, rb.lists, rb.bases
        );
        for cell in [[0, 1, 0], [1, 1, 1], [0, 2, 0], [2, 0, -2]] {
            let slot = rb.slot(0, cell);
            let (off, tag) = rb.probe(slot);
            let up = rb.irradiance(slot, 2, 2);
            let rays: Vec<String> = rb.rays[slot as usize * 64..slot as usize * 64 + 8]
                .iter()
                .map(|r| format!("{:.2}/{:.2}", r[1], r[3]))
                .collect();
            eprintln!("cell {cell:?} offset {off:?} tag {tag:?} irr {up:?} rays {rays:?}");
        }
    }
    let interior = linear_luma(&in_px, 40, 50, 120, 90);
    dump("sealed_outside", &out_px);
    dump("sealed_inside", &in_px);
    eprintln!("sealed room: exterior ground {exterior:.4}, interior floor {interior:.6} (linear luma)");
    assert!(exterior > 0.1, "the exterior ground must be lit ({exterior})");
    assert!(
        interior <= 0.01 * exterior,
        "light leaks into the sealed room: interior {interior:.5} vs exterior {exterior:.4} ({:.2} %)",
        interior / exterior * 100.0
    );
}

#[test]
fn open_ground_reads_the_sky_and_a_sunlit_trench_wall_lights_the_floor() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    // Sun off: the ground sees only a uniform sky of `ambient_lux`. With probes the ground
    // must read that sky (E/π = ambient/π) like the flat ambient term it replaces.
    let ground = vec![block([0.0, -0.25, 0.0], [40.0, 0.25, 40.0], 0)];
    let mut open = scene([0.0, 2.0, 6.0], [0.0, 0.0, -2.0], ground.clone(), vec![grey(0.6)]);
    open.directional = sun(50.0, 0.0);
    // The sky map: the forward pass's sky irradiance and the probes' misses come from the
    // same sky.
    open.ibl = true;
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(test_settings());
    renderer.set_ddgi_enabled(Some(true));
    let probes = linear_luma(&converge(&mut renderer, &device, &open, 40), 40, 60, 120, 92);
    let mut flat = FrameRenderer::new();
    flat.set_ddgi_enabled(Some(false));
    let ambient = linear_luma(&converge(&mut flat, &device, &open, 4), 40, 60, 120, 92);
    eprintln!("open ground under a uniform sky: probes {probes:.4}, flat ambient {ambient:.4}");
    assert!(
        (probes / ambient - 1.0).abs() < 0.15,
        "probe irradiance under an empty sky must match the sky ({probes} vs {ambient})"
    );

    // A trench: 6 m wide, 4 m walls, long in x, the sun low across it. The far wall's
    // inner face is sunlit, the floor is in the near wall's shadow, and with no sky the
    // only light on the floor is that wall's bounce: the probes carry it, the flat
    // ambient cannot.
    let mut meshes = ground;
    meshes.push(block([0.0, 2.0, -3.15], [15.0, 2.0, 0.15], 1));
    meshes.push(block([0.0, 2.0, 3.15], [15.0, 2.0, 0.15], 1));
    let mut walled = scene(
        [0.0, 1.7, 2.4],
        [0.0, 0.3, 0.0],
        meshes,
        vec![grey(0.6), grey(0.9)],
    );
    // Travelling −z and down at 35°: over the +z wall onto the −z wall's inner face.
    walled.directional = DirectionalLightUniform::new([0.0, -0.574, -0.819], [1.0; 3], 100_000.0);
    walled.ambient_lux = 0.0;
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(test_settings());
    renderer.set_ddgi_enabled(Some(true));
    let lit = linear_luma(&converge(&mut renderer, &device, &walled, 40), 60, 60, 100, 92);
    let mut flat = FrameRenderer::new();
    flat.set_ddgi_enabled(Some(false));
    let dark = linear_luma(&converge(&mut flat, &device, &walled, 4), 60, 60, 100, 92);
    eprintln!("shadowed ground before a sunlit wall: probes {lit:.5}, no probes {dark:.5}");
    assert!(
        lit > dark + 0.005,
        "the probes must add the sunlit ground's bounce ({lit} vs {dark})"
    );
}

/// Mean and 99th percentile of |Δ luma| (0–255) between two frames.
fn frame_delta(a: &[u8], b: &[u8]) -> (f32, f32) {
    let luma = |p: &[u8]| 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]);
    let mut d: Vec<f32> = a
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .map(|(x, y)| (luma(x) - luma(y)).abs())
        .collect();
    let mean = d.iter().sum::<f32>() / d.len().max(1) as f32;
    d.sort_by(f32::total_cmp);
    (mean, d[(d.len() * 99 / 100).min(d.len() - 1)])
}

/// A sunlit trench of mesh walls (probes carry the wall's bounce onto the shadowed floor).
fn trench_scene(eye_x: f32) -> LabScene {
    let meshes = vec![
        block([0.0, -0.25, 0.0], [60.0, 0.25, 60.0], 0),
        block([0.0, 2.0, -3.15], [40.0, 2.0, 0.15], 1),
        block([0.0, 2.0, 3.15], [40.0, 2.0, 0.15], 1),
        block([0.0, 1.0, 0.0], [0.6, 1.0, 0.6], 1),
    ];
    let mut s = scene(
        [eye_x, 1.7, 2.4],
        [eye_x, 0.3, 0.0],
        meshes,
        vec![grey(0.6), grey(0.85)],
    );
    s.directional = DirectionalLightUniform::new([0.2, -0.574, -0.8], [1.0; 3], 100_000.0);
    s.ambient_lux = 2_000.0;
    s
}

#[test]
fn converged_probes_do_not_flicker_and_a_round_trip_returns_the_same_light() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    // No screen-space reuse (that would hide probe noise) and no TAA: raw frames.
    let render = |renderer: &mut FrameRenderer, lab: &LabScene| -> Vec<u8> {
        let mut q = quality();
        q.screen_space_gi = false;
        let frame = lab.as_frame_scene_with_quality(q);
        renderer
            .render_offscreen(&device, &frame)
            .unwrap_or_else(|err| panic!("frame: {err}"))
            .pixels
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(DdgiSettings {
        update_divisor: 2,
        ..test_settings()
    });
    renderer.set_ddgi_enabled(Some(true));
    let home = trench_scene(0.0);
    let mut last = Vec::new();
    for _ in 0..60 {
        last = render(&mut renderer, &home);
    }
    // Held camera: the probes keep re-tracing (with a new ray rotation every frame).
    let mut worst = (0.0f32, 0.0f32);
    for _ in 0..8 {
        let next = render(&mut renderer, &home);
        let (mean, p99) = frame_delta(&last, &next);
        worst = (worst.0.max(mean), worst.1.max(p99));
        last = next;
    }
    eprintln!(
        "held camera, consecutive frames: mean |Δ luma| ≤ {:.3}, p99 ≤ {:.2}",
        worst.0, worst.1
    );
    assert!(
        worst.0 < 0.5 && worst.1 < 3.0,
        "converged probes flicker: {worst:?}"
    );
    // Walk 24 m down the trench and back (the volumes scroll both ways), then settle: the
    // light returns to what it was (the coarse cascade, re-traced every 4th frame, keeps a
    // memory of the trip for a few of its time constants).
    let start = last.clone();
    for i in 1..=96 {
        render(&mut renderer, &trench_scene(i as f32 * 0.25));
    }
    for i in (0..96).rev() {
        render(&mut renderer, &trench_scene(i as f32 * 0.25));
    }
    for _ in 0..180 {
        last = render(&mut renderer, &home);
    }
    let (mean, p99) = frame_delta(&start, &last);
    eprintln!("after a 24 m round trip: mean |Δ luma| {mean:.3}, p99 {p99:.2}");
    // Relocated probes keep their offsets (relocation is incremental), so the field is a
    // little path-dependent: about one luma level on this scene.
    assert!(
        mean < 2.0 && p99 < 8.0,
        "the light did not return after a round trip ({mean}, {p99})"
    );
}

/// A kilometre ground mesh with relief (here 4 m thick) would be voxelised in 125 m steps
/// along the ground (a brick has at most 40 voxels per axis): its surface sat tens of metres
/// off and buried the probes around it, so it is not traced (`DdgiSettings::max_proxy_voxel`).
/// A 2 m block on it still is. The bricks bake on the task pool here; an offscreen frame waits
/// for them, so the first frame traces them.
#[test]
fn a_kilometre_ground_mesh_is_not_traced_but_a_block_on_it_is() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let meshes = vec![
        block([0.0, -2.0, 0.0], [2500.0, 2.0, 2500.0], 0),
        block([0.0, 1.0, -6.0], [1.0, 1.0, 1.0], 0),
    ];
    let lab = scene([0.0, 1.7, 0.0], [0.0, 1.0, -6.0], meshes, vec![grey(0.5)]);
    let instances = |max_proxy_voxel: f32| {
        let mut renderer = FrameRenderer::new();
        renderer.set_ddgi_settings(DdgiSettings {
            max_proxy_voxel,
            bake_on_caller: false,
            ..test_settings()
        });
        renderer.set_ddgi_enabled(Some(true));
        converge(&mut renderer, &device, &lab, 1);
        assert_eq!(
            renderer.ddgi_pending_bakes(),
            0,
            "an offscreen frame waits for its bricks"
        );
        renderer
            .ddgi_read_back(&device)
            .unwrap_or_else(|err| panic!("read back: {err}"))
            .unwrap_or_else(|| panic!("the probes did not run"))
            .instances
    };
    assert_eq!(
        instances(DdgiSettings::default().max_proxy_voxel),
        1,
        "only the block is traced"
    );
    assert_eq!(instances(1000.0), 2, "without the limit the ground is traced too");
}

/// A kilometre plate (0.5 m thick: a floor, a pad, a studio ground) is traced however long it
/// is: its brick resolves the surface through its thin axis, so the probes over it are not
/// buried (ADR-169). Untraced, probe rays that go down read the sky map's sunlit ground under
/// every hull.
#[test]
fn a_kilometre_plate_is_traced_and_the_probes_over_it_stay_active() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let meshes = vec![
        block([0.0, -0.25, 0.0], [2500.0, 0.25, 2500.0], 0),
        block([0.0, 1.0, -6.0], [1.0, 1.0, 1.0], 0),
    ];
    let lab = scene([0.0, 1.7, 0.0], [0.0, 1.0, -6.0], meshes, vec![grey(0.5)]);
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(test_settings());
    renderer.set_ddgi_enabled(Some(true));
    converge(&mut renderer, &device, &lab, 12);
    let rb = renderer
        .ddgi_read_back(&device)
        .unwrap_or_else(|err| panic!("read back: {err}"))
        .unwrap_or_else(|| panic!("the probes did not run"));
    assert_eq!(rb.instances, 2, "the plate and the block are traced");
    // Cascade 0 (1 m): the probes 1–3 m over the plate around the eye.
    let mut checked = 0;
    for y in 1..=3 {
        for z in -3..=3 {
            for x in -3..=3 {
                let (offset, tag) = rb.probe(rb.slot(0, [x, y, z]));
                if tag[..3] != [x, y, z] {
                    continue;
                }
                checked += 1;
                assert!(
                    offset[3] > 0.5,
                    "probe ({x}, {y}, {z}) over the plate reads as buried"
                );
            }
        }
    }
    assert!(
        checked > 100,
        "only {checked} probes over the plate were in the volume"
    );
}

/// A flat quad of `half` metres at y = 0, facing up: two triangles without thickness (a studio ground).
fn ground_quad(half: f32, material: usize) -> LabMesh {
    let vertex = |x: f32, z: f32| LabVertex::from_pos_normal_uv([x, 0.0, z], [0.0, 1.0, 0.0], [0.0; 2]);
    LabMesh {
        vertices: vec![
            vertex(-half, -half),
            vertex(-half, half),
            vertex(half, half),
            vertex(half, -half),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    }
}

/// ADR-5716: a probe over a flat ground quad (no thickness) sees the quad under it. The quad's brick held +∞
/// in every sample more than 0.4 mm before its plane (the closest-point search of `ddgi_bake.rs` started
/// outside its grid and visited no cell), so a probe ray toward the quad's front left the brick with one
/// infinite step: on lavapipe no ray from above stopped on the quad, on an NVIDIA card only the steep ones did
/// (22 of a probe's 32 downward rays), and the two traced different scenes (`studio_measure`, the visibility
/// buffer's wind pair, whose plants are such cards).
#[test]
fn a_probe_over_a_flat_ground_quad_sees_it() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let meshes = vec![ground_quad(40.0, 0), block([0.0, 1.0, -6.0], [1.0, 1.0, 1.0], 0)];
    let lab = scene([0.0, 1.7, 0.0], [0.0, 1.0, -6.0], meshes, vec![grey(0.5)]);
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(test_settings());
    renderer.set_ddgi_enabled(Some(true));
    converge(&mut renderer, &device, &lab, 12);
    let rb = renderer
        .ddgi_read_back(&device)
        .unwrap_or_else(|err| panic!("read back: {err}"))
        .unwrap_or_else(|| panic!("the probes did not run"));
    assert_eq!(rb.instances, 2, "the quad and the block are traced");
    // Probes one and two metres over the quad, away from the block.
    for cell in [[2, 1, 2], [-3, 1, 1], [1, 2, 3], [-2, 2, 0]] {
        let slot = rb.slot(0, cell);
        let (offset, tag) = rb.probe(slot);
        assert_eq!(tag[..3], cell, "the probe is in the volume");
        assert!(offset[3] > 0.5, "probe {cell:?} over the quad reads as buried");
        let rays = &rb.rays[slot as usize * 64..slot as usize * 64 + 64];
        let stopped: Vec<f32> = rays
            .iter()
            .map(|r| r[3])
            .filter(|t| (0.0..1.0e8).contains(t))
            .collect();
        let backs = rays.iter().filter(|r| r[3] < 0.0).count();
        eprintln!(
            "probe {cell:?}: {} of 64 rays stopped ({} back faces), nearest {:?}",
            stopped.len(),
            backs,
            stopped.iter().copied().reduce(f32::min)
        );
        assert_eq!(backs, 0, "a probe over the quad sees no back face");
        // Half the rays point down. The ones within a few degrees of the horizon reach the quad past the
        // march's 64 steps (or past the coarser cascade's reach) and read the sky.
        assert!(
            stopped.len() >= 26,
            "probe {cell:?}: only {} of 64 rays stopped on the quad under it",
            stopped.len()
        );
        // Nothing lies nearer than the quad, straight down.
        let height = cell[1] as f32;
        assert!(
            stopped.iter().all(|t| *t >= 0.98 * height),
            "probe {cell:?}: a ray stopped before the quad"
        );
        // The hits are the sunlit ground: its albedo 0.5 under the sun's 100 klx at 50 degrees, over pi
        // (the probes' own bounce adds to it). The quad does not shadow itself: a hit's sun shadow ray
        // starts 2 cm over a brick a millimetre thick.
        let sunlit = 0.5 * 100_000.0 * 50f32.to_radians().sin() / std::f32::consts::PI;
        let lit = rays
            .iter()
            .filter(|r| (0.0..1.0e8).contains(&r[3]) && luminance([r[0], r[1], r[2]]) > 0.8 * sunlit)
            .count();
        assert!(
            lit >= 24,
            "probe {cell:?}: only {lit} of {} hits on the quad carry the sun's bounce ({sunlit:.0})",
            stopped.len()
        );
    }
}

/// A probe ray that leaves its own cascade's mesh reach (spacing × the largest probe count per axis: 12 m
/// here) goes on through the coarser cascade's instance list (ADR-5374). It used to read the sky from there
/// on, so a probe 2 m over a kilometre plate saw ground under 42 % of its rays instead of half, and none of
/// it farther than 12 m: the far sunlit ground a tunnel or a doorway is lit by was sky to the probes.
#[test]
fn a_probe_ray_past_its_cascades_reach_still_finds_the_ground() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let meshes = vec![block([0.0, -0.25, 0.0], [2500.0, 0.25, 2500.0], 0)];
    let lab = scene([0.0, 1.7, 0.0], [0.0, 1.0, -6.0], meshes, vec![grey(0.5)]);
    let settings = test_settings();
    let reach = settings.spacing * settings.dims.iter().copied().max().unwrap_or(1) as f32;
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(settings);
    renderer.set_ddgi_enabled(Some(true));
    converge(&mut renderer, &device, &lab, 12);
    let rb = renderer
        .ddgi_read_back(&device)
        .unwrap_or_else(|err| panic!("read back: {err}"))
        .unwrap_or_else(|| panic!("the probes did not run"));
    // Cascade 0 (1 m): the probes 1 to 3 m over the plate around the eye, 2 m on average.
    let (mut rays, mut hits, mut far, mut farthest) = (0usize, 0usize, 0usize, 0.0f32);
    for y in 1..=3 {
        for z in -3..=3 {
            for x in -3..=3 {
                let slot = rb.slot(0, [x, y, z]) as usize;
                let (offset, tag) = rb.probe(slot as u32);
                if tag[..3] != [x, y, z] || offset[3] < 0.5 {
                    continue;
                }
                for ray in &rb.rays[slot * 64..slot * 64 + 64] {
                    rays += 1;
                    if ray[3] >= 0.0 && ray[3] < 1.0e8 {
                        hits += 1;
                        farthest = farthest.max(ray[3]);
                        if ray[3] > reach * 1.05 {
                            far += 1;
                        }
                    }
                }
            }
        }
    }
    assert!(rays > 100 * 64, "only {rays} rays over the plate were read");
    let share = hits as f32 / rays as f32;
    eprintln!(
        "rays over the plate: {rays}, ground hits {share:.4}, past {reach} m: {far}, farthest {farthest:.1} m"
    );
    assert!(
        far > 0 && farthest > 2.0 * reach,
        "no probe ray found the plate past its cascade's {reach} m reach (farthest hit {farthest:.1} m)"
    );
    // Half the sphere looks down. Within the coarser cascade's 36 m a probe 2 m up sees the plate under
    // (1 − 2 / 36) / 2 = 47 % of its rays; stopped at 12 m, 42 %.
    assert!(
        share > 0.45,
        "the plate fills {share:.4} of the probes' rays: rays past {reach} m read the sky"
    );
}

/// On a tier layout (`DdgiSettings::visibility_blend`, ADR-5374) the lookup's normal and view bias move the
/// point the visibility test looks at, not the point the probes are weighted at (Majercik et al. 2019, listing
/// 1). Weighted at the biased point, a surface took a share of its light from the probes toward the camera, more
/// of it the larger the bias: in a room lit through a door the floor before the door read the probes farther
/// inside, and its light fell by a third from a view bias of 0.3 to 0.9. What is left is the visibility test's
/// own share (4 % on an RTX 5080).
#[test]
fn the_lookup_bias_steers_only_the_visibility_test() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let (near, far) = (
        door_room_floor(&device, 0.3, true),
        door_room_floor(&device, 0.9, true),
    );
    eprintln!("door room floor under the sky: view bias 0.3 {near:.5}, 0.9 {far:.5}");
    assert!(near > 0.0, "the probes lit the floor");
    assert!(
        (far / near - 1.0).abs() < 0.1,
        "the floor's light follows the view bias: {near:.5} at 0.3, {far:.5} at 0.9"
    );
}

/// The floor before the door of a room lit through that door by the sky alone, at a view bias, on the test
/// layout with or without `DdgiSettings::visibility_blend` (linear luma).
fn door_room_floor(device: &RenderDevice, view_bias: f32, visibility_blend: bool) -> f32 {
    // A 6 x 3 x 6 m room (20 cm walls) with a 1.6 m door in its +z wall, the camera inside looking at the
    // floor before the door: the light falls off fast from the door inward.
    let t = 0.1;
    let meshes = vec![
        block([0.0, -0.25, 0.0], [30.0, 0.25, 30.0], 0),
        block([3.0 + t, 1.5, 0.0], [t, 1.5, 3.0 + 2.0 * t], 0),
        block([-3.0 - t, 1.5, 0.0], [t, 1.5, 3.0 + 2.0 * t], 0),
        block([0.0, 1.5, -3.0 - t], [3.0, 1.5, t], 0),
        block([-1.9, 1.5, 3.0 + t], [1.1, 1.5, t], 0),
        block([1.9, 1.5, 3.0 + t], [1.1, 1.5, t], 0),
        block([0.0, 3.0 + t, 0.0], [3.0 + 2.0 * t, t, 3.0 + 2.0 * t], 0),
    ];
    let mut lab = scene([0.0, 1.6, -1.5], [0.0, 0.0, 1.5], meshes, vec![grey(0.6)]);
    // The sky alone: no sun patch, so the floor holds indirect light only.
    lab.directional = sun(50.0, 0.0);
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(DdgiSettings {
        view_bias,
        visibility_blend,
        ..test_settings()
    });
    renderer.set_ddgi_enabled(Some(true));
    linear_luma(&converge(&mut renderer, device, &lab, 60), 40, 50, 120, 90)
}

/// ADR-5506: the lookup of ADR-5374 is every layout's. A layout without `DdgiSettings::visibility_blend` (the
/// probes' defaults, the validation plates) kept the weights at the biased point until then, and the same floor
/// lost a third of its light from a view bias of 0.3 to 0.9.
#[test]
fn a_layout_without_the_visibility_blend_weighs_the_probes_at_the_surface_point() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let (near, far) = (
        door_room_floor(&device, 0.3, false),
        door_room_floor(&device, 0.9, false),
    );
    eprintln!("door room floor, no visibility blend: view bias 0.3 {near:.5}, 0.9 {far:.5}");
    assert!(near > 0.0, "the probes lit the floor");
    assert!(
        (far / near - 1.0).abs() < 0.1,
        "the floor's light follows the view bias: {near:.5} at 0.3, {far:.5} at 0.9"
    );
}

/// ADR-5506: a new probe re-traces every frame for its first sixteen updates (eight before), then joins its
/// cascade's cadence. Probes in open air over a ground, on a layout whose settled probes re-trace every eighth
/// frame: sixteen frames give sixteen updates, and the next eight frames give one more.
#[test]
fn a_new_probe_traces_every_frame_for_its_first_sixteen_updates() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let meshes = vec![block([0.0, -0.25, 0.0], [30.0, 0.25, 30.0], 0)];
    let lab = scene([0.0, 1.7, 0.0], [0.0, 1.0, -6.0], meshes, vec![grey(0.5)]);
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(DdgiSettings {
        update_divisor: 8,
        ..test_settings()
    });
    renderer.set_ddgi_enabled(Some(true));
    let mut updates = |frames: u32| {
        converge(&mut renderer, &device, &lab, frames);
        let rb = renderer
            .ddgi_read_back(&device)
            .unwrap_or_else(|err| panic!("read back: {err}"))
            .unwrap_or_else(|| panic!("the probes ran"));
        // Finest probes two and three metres over the ground: nothing near enough to move them.
        [[0, 2, 0], [1, 3, -1], [-2, 2, 2]].map(|cell| {
            let (offset, tag) = rb.probe(rb.slot(0, cell));
            assert!(
                tag[..3] == cell && offset[3] > 0.5,
                "probe {cell:?} is traced and active"
            );
            tag[3]
        })
    };
    let young = updates(16);
    let later = updates(8);
    eprintln!("updates of three open-air probes after 16 frames {young:?}, after 24 {later:?}");
    assert_eq!(
        young, [16; 3],
        "a new probe traces every frame for sixteen updates"
    );
    assert_eq!(
        later, [17; 3],
        "after its sixteenth update a probe re-traces every `update_divisor` frames"
    );
}

/// Luminance of a linear color.
fn luminance(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// ADR-5506: between the probes each face of the lookup is interpolated as its cube root (a lightness space;
/// Majercik et al. 2019, listing 1, blend at a gamma of 2). A dark floor runs from under the open sky to under
/// a low dark roof, so the probes of one row hold several times the light toward +y of the next row's. Three
/// texels of the screen lookup along that metre (near the bright row, in the middle, near the dark row): the
/// middle one reads far under the chord of the outer two (0.70 of it on an RTX 5080; 0.77 with a square root),
/// where a linear blend of the probes, as the lookup had before, reads 0.95 of it.
#[test]
fn the_lookup_blends_the_probes_in_a_lightness_space() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    // A ground, and a roof slab 0.3 m over it from x = 0.6 on: the probes at x = 0 stand under the sky, the
    // ones at x = 1 under the roof. The view looks along +x at the floor between them, from low enough to
    // see under the roof.
    let meshes = vec![
        block([0.0, -0.25, 0.0], [30.0, 0.25, 30.0], 0),
        block([3.6, 0.4, 0.0], [3.0, 0.1, 6.0], 0),
    ];
    let (eye, target) = ([-1.5f32, 1.0, 0.5], [0.5f32, 0.0, 0.5]);
    // Nearly black surfaces: the light under the roof is the sky through the slit, not a bounce.
    let mut lab = scene(eye, target, meshes, vec![grey(0.05)]);
    lab.directional = sun(50.0, 0.0);
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(test_settings());
    renderer.set_ddgi_enabled(Some(true));
    converge(&mut renderer, &device, &lab, 96);
    let rb = renderer
        .ddgi_read_back(&device)
        .unwrap_or_else(|err| panic!("read back: {err}"))
        .unwrap_or_else(|| panic!("the probes ran"));
    let (w, h, texels) = rb
        .screen_irradiance()
        .unwrap_or_else(|| panic!("the screen lookup ran"));
    assert_eq!((w, h), (W / 2, H / 2), "the lookup runs at half resolution");
    // Lookup texel (W / 4, row) holds pixel (W / 2, 2 row): where that pixel's ray meets the floor. The view
    // looks along +x at z = 0.5, so the middle column runs along x between the probe rows z = 0 and z = 1.
    let forward = {
        let d = [target[0] - eye[0], target[1] - eye[1], target[2] - eye[2]];
        let l = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        [d[0] / l, d[1] / l, d[2] / l]
    };
    // right = forward x (0, 1, 0), normalized; up = right x forward.
    let right = {
        let r = [-forward[2], 0.0, forward[0]];
        let l = (r[0] * r[0] + r[2] * r[2]).sqrt();
        [r[0] / l, 0.0, r[2] / l]
    };
    let up = [
        right[1] * forward[2] - right[2] * forward[1],
        right[2] * forward[0] - right[0] * forward[2],
        right[0] * forward[1] - right[1] * forward[0],
    ];
    let tan = (0.5f32 * lab.fov_y).tan();
    let floor_x = |row: u32| {
        let ndc_x = (W as f32 + 1.0) / W as f32 - 1.0;
        let ndc_y = 1.0 - (4.0 * row as f32 + 1.0) / H as f32;
        let ray: [f32; 3] = std::array::from_fn(|a| {
            forward[a] + right[a] * (tan * ndc_x * W as f32 / H as f32) + up[a] * (tan * ndc_y)
        });
        (ray[1] < -1e-3).then(|| eye[0] - ray[0] * eye[1] / ray[1])
    };
    // (position between the probe rows, light toward +y) of the middle column's floor texels.
    let samples: Vec<(f32, f32)> = (0..h)
        .filter_map(|row| {
            let x = floor_x(row)?;
            let t = texels[(row * w + W / 4) as usize];
            (x > 0.02 && x < 0.98 && t.coverage > 0.5).then(|| (x, luminance(t.e[2])))
        })
        .collect();
    let nearest = |x: f32| {
        samples
            .iter()
            .copied()
            .min_by(|a, b| (a.0 - x).abs().total_cmp(&(b.0 - x).abs()))
            .unwrap_or_else(|| panic!("the middle column holds floor texels"))
    };
    let (bright, middle, dark) = (nearest(0.15), nearest(0.5), nearest(0.85));
    assert!(
        bright.0 < 0.3 && (middle.0 - 0.5).abs() < 0.15 && dark.0 > 0.7,
        "texels near both probe rows and between them: {samples:?}"
    );
    let k = (middle.0 - bright.0) / (dark.0 - bright.0);
    let chord = bright.1 + (dark.1 - bright.1) * k;
    eprintln!(
        "floor from the sky to under the roof: {:.1} at x {:.3}, {:.1} at {:.3}, {:.1} at {:.3}; the middle \
         reads {:.3} of the outer two's chord ({chord:.1})",
        bright.1,
        bright.0,
        middle.1,
        middle.0,
        dark.1,
        dark.0,
        middle.1 / chord
    );
    assert!(
        bright.1 > 3.0 * dark.1,
        "the roof makes a steep gradient between the probe rows: {bright:?} to {dark:?}"
    );
    assert!(
        middle.1 < 0.85 * chord && middle.1 > 0.5 * chord,
        "between the probe rows the lookup reads {}, the chord of the outer two is {chord}",
        middle.1
    );
}

/// ADR-5506: a probe too near a front face is pushed away along the mean of its near rays (the surface's
/// normal over a face), not along the single nearest one, which lies up to ten degrees off the normal in a
/// direction that follows the frame's ray rotation. A floor 5 mm under a probe plane: every probe of that plane
/// ends straight over its lattice point, at the least distance from the floor. Before, each slid a centimetre
/// or two along the floor.
#[test]
fn a_probe_near_a_floor_is_pushed_straight_off_it() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    // The ground's top 5 mm under y = 0.
    let meshes = vec![block([0.0, -0.255, 0.0], [30.0, 0.25, 30.0], 0)];
    let lab = scene([0.0, 1.7, 0.0], [0.0, 1.0, -6.0], meshes, vec![grey(0.5)]);
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(test_settings());
    renderer.set_ddgi_enabled(Some(true));
    converge(&mut renderer, &device, &lab, 24);
    let rb = renderer
        .ddgi_read_back(&device)
        .unwrap_or_else(|err| panic!("read back: {err}"))
        .unwrap_or_else(|| panic!("the probes ran"));
    let least = test_settings().min_frontface;
    let (mut slide, mut low, mut high) = (0.0f32, f32::MAX, f32::MIN);
    for x in -3..=3 {
        for z in -3..=3 {
            let cell = [x, 0, z];
            let (offset, tag) = rb.probe(rb.slot(0, cell));
            assert!(
                tag[..3] == cell && offset[3] > 0.5,
                "probe {cell:?} is traced and active"
            );
            slide = slide.max(offset[0].abs()).max(offset[2].abs());
            low = low.min(offset[1]);
            high = high.max(offset[1]);
        }
    }
    eprintln!(
        "49 probes 5 mm over a floor: lifted {low:.4} to {high:.4} m, slid up to {slide:.4} m along it"
    );
    assert!(
        low > least - 0.02 && high < least + 0.01,
        "the probes end the least distance ({least} spacings) over the floor: {low} to {high}"
    );
    assert!(
        slide < 0.004,
        "a probe pushed off a floor slid {slide} m along it"
    );
}

/// ADR-5457: a probe before a wall also holds the light at the wall. A tunnel takes its light through its
/// mouths, so its walls are lit at a grazing angle, where the irradiance climbs steeply toward the wall: half
/// a cell in front of it a point receives clearly less. With the walls halfway between two probe planes the
/// lookup read exactly that point (the probes behind the wall do not see it, so the layer in front stood for
/// the wall). On a tier layout the axis texels of odd parity now hold the irradiance at the surface behind the
/// probe, estimated from the probe's own rays, and the lookup reads them for a point on that surface. Before,
/// those texels were copies of the even ones.
#[test]
fn a_probe_before_a_wall_holds_the_light_at_the_wall() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    // A tunnel 16 m long in x, 3 m wide and tall inside (20 cm walls and roof) on a large ground, the sun on the
    // ground outside both mouths: its walls lie at z = +-1.5, between the probe planes at 1 and 2 m.
    let t = 0.1;
    let meshes = vec![
        block([0.0, -0.25, 0.0], [40.0, 0.25, 40.0], 0),
        block([0.0, 1.5, 1.5 + t], [8.0, 1.5, t], 0),
        block([0.0, 1.5, -1.5 - t], [8.0, 1.5, t], 0),
        block([0.0, 3.0 + t, 0.0], [8.0, t, 1.5 + 2.0 * t], 0),
    ];
    let mut lab = scene([-2.0, 1.5, 0.6], [6.0, 1.0, -0.3], meshes, vec![grey(0.5)]);
    lab.directional = DirectionalLightUniform::new([-0.45, -0.819, -0.355], [1.0; 3], 100_000.0);
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(DdgiSettings {
        visibility_blend: true,
        ..test_settings()
    });
    renderer.set_ddgi_enabled(Some(true));
    converge(&mut renderer, &device, &lab, 96);
    let rb = renderer
        .ddgi_read_back(&device)
        .unwrap_or_else(|err| panic!("read back: {err}"))
        .unwrap_or_else(|| panic!("the probes ran"));
    // A probe's -z lobe (the +z wall's normal): the texel at the probe (0, 0) and the one at the surface behind
    // it (5, 0), summed over the channels.
    let lobes = |cell: [i32; 3]| {
        let slot = rb.slot(0, cell);
        let (offset, tag) = rb.probe(slot);
        assert!(
            tag[..3] == cell && offset[3] > 0.5,
            "probe {cell:?} is traced and active"
        );
        let sum = |x: u32| rb.irradiance(slot, x, 0).iter().sum::<f32>();
        (sum(0), sum(5))
    };
    // A metre up, half a metre before the +z wall, 8 m from either mouth; and the probe on the centre line
    // beside it, 1.5 m from both walls (no surface within its reach along z).
    let (at_probe, at_wall) = lobes([0, 1, 1]);
    let (centre, centre_odd) = lobes([0, 1, 0]);
    eprintln!(
        "tunnel probe before the wall, -z lobe: at the probe {at_probe:.2}, at the wall {at_wall:.2} ({:.3} x); \
         centre line {centre:.2} / {centre_odd:.2}",
        at_wall / at_probe
    );
    assert!(at_probe > 0.0 && centre > 0.0, "the probes hold light");
    assert!(
        at_wall > at_probe * 1.05 && at_wall < at_probe * 1.8,
        "the wall's light against the light half a metre before it: {at_wall} / {at_probe}"
    );
    assert!(
        (centre_odd / centre - 1.0).abs() < 1e-3,
        "a probe with no surface behind it keeps its own lobe in both texels: {centre} / {centre_odd}"
    );
}

/// ADR-5506: a probe ray's hit reflects as the forward pass's dielectric (Lambert under 1 - F, a GGX lobe),
/// not as a Lambertian surface of the mesh's albedo. Light that travels down a tunnel leaves every wall at a
/// grazing angle, where the dielectric reflects more than its albedo: the probes deep inside hold more light
/// than with the same albedo reflected evenly (the metallic share of a hit keeps the even reflection, so a
/// fully metallic material of the same base color is that control). Before, the two fields were equal.
#[test]
fn a_rough_dielectric_carries_more_light_down_a_tunnel_than_its_albedo_alone() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let t = 0.1;
    let meshes = || {
        vec![
            block([0.0, -0.25, 0.0], [40.0, 0.25, 40.0], 0),
            block([0.0, 1.5, 1.5 + t], [8.0, 1.5, t], 0),
            block([0.0, 1.5, -1.5 - t], [8.0, 1.5, t], 0),
            block([0.0, 3.0 + t, 0.0], [8.0, t, 1.5 + 2.0 * t], 0),
        ]
    };
    // The light the probes on the tunnel's centre line hold, 6 m and more from either mouth: every texel of
    // their tiles, summed.
    let inside = |material: LabMaterial| {
        let mut lab = scene([-2.0, 1.5, 0.6], [6.0, 1.0, -0.3], meshes(), vec![material]);
        lab.directional = DirectionalLightUniform::new([-0.45, -0.819, -0.355], [1.0; 3], 100_000.0);
        let mut renderer = FrameRenderer::new();
        renderer.set_ddgi_settings(test_settings());
        renderer.set_ddgi_enabled(Some(true));
        converge(&mut renderer, &device, &lab, 96);
        let rb = renderer
            .ddgi_read_back(&device)
            .unwrap_or_else(|err| panic!("read back: {err}"))
            .unwrap_or_else(|| panic!("the probes ran"));
        let mut sum = 0.0f32;
        for x in -2..=2 {
            let cell = [x, 1, 0];
            let slot = rb.slot(0, cell);
            let (offset, tag) = rb.probe(slot);
            assert!(
                tag[..3] == cell && offset[3] > 0.5,
                "probe {cell:?} is traced and active"
            );
            for ty in 0..6 {
                for tx in 0..6 {
                    sum += luminance(rb.irradiance(slot, tx, ty));
                }
            }
        }
        sum
    };
    let dielectric = inside(grey(0.5));
    let even = inside(LabMaterial {
        metallic: 1.0,
        ..grey(0.5)
    });
    eprintln!(
        "tunnel centre line, light in the probes: dielectric {dielectric:.1}, even reflection {even:.1} ({:.4} x)",
        dielectric / even
    );
    assert!(even > 0.0, "the probes hold light");
    assert!(
        dielectric > 1.008 * even && dielectric < 1.15 * even,
        "a rough dielectric tunnel holds {dielectric}, the same albedo reflected evenly {even}"
    );
}

/// ADR-5506: a probe ray's hit is where the ray crosses a surface, so an opening lets in the light of its own
/// width. A dark room under the sky with a 0.4 m slit along its roof: the probe a metre under the slit holds,
/// toward +y, the sky's radiance times the slit's form factor (the cosine-weighted share of the hemisphere its
/// upper opening fills): 1.008 times it on an RTX 5080. Before, a ray stopped within half a voxel of any
/// surface, the slit read that much narrower on each side, and the probe held 0.909 times it.
#[test]
fn a_slit_lets_in_the_light_of_its_own_width() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    // A room 6 m wide, 6 m long and 2 m tall inside (20 cm walls and roof) on a ground; the roof is two slabs
    // that leave x = -0.2 .. 0.2 open over the room's whole length.
    let (t, half_slit, roof, half_len) = (0.1f32, 0.2f32, 2.0f32, 3.0f32);
    let slab = 0.5 * (3.0 + 2.0 * t - half_slit);
    let meshes = vec![
        block([0.0, -0.25, 0.0], [30.0, 0.25, 30.0], 0),
        block(
            [half_slit + slab, roof + t, 0.0],
            [slab, t, half_len + 2.0 * t],
            0,
        ),
        block(
            [-half_slit - slab, roof + t, 0.0],
            [slab, t, half_len + 2.0 * t],
            0,
        ),
        block([3.0 + t, 0.5 * roof, 0.0], [t, 0.5 * roof, half_len + 2.0 * t], 0),
        block(
            [-3.0 - t, 0.5 * roof, 0.0],
            [t, 0.5 * roof, half_len + 2.0 * t],
            0,
        ),
        block([0.0, 0.5 * roof + t, half_len + t], [3.0, 0.5 * roof + t, t], 0),
        block([0.0, 0.5 * roof + t, -half_len - t], [3.0, 0.5 * roof + t, t], 0),
    ];
    // Nearly black surfaces: what the probe holds toward +y is the sky through the slit.
    let mut lab = scene([1.5, 1.2, 2.0], [0.0, 1.0, 0.0], meshes, vec![grey(0.03)]);
    lab.directional = sun(50.0, 0.0);
    let sky = lab.ambient_lux / std::f32::consts::PI;
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(test_settings());
    renderer.set_ddgi_enabled(Some(true));
    converge(&mut renderer, &device, &lab, 160);
    let rb = renderer
        .ddgi_read_back(&device)
        .unwrap_or_else(|err| panic!("read back: {err}"))
        .unwrap_or_else(|| panic!("the probes ran"));
    // The probes under the slit's middle line, a metre under the roof: their +y lobe at the probe.
    let mut held = 0.0f32;
    let cells = [[0, 1, -1], [0, 1, 0], [0, 1, 1]];
    for cell in cells {
        let slot = rb.slot(0, cell);
        let (offset, tag) = rb.probe(slot);
        assert!(
            tag[..3] == cell && offset[3] > 0.5 && offset[..3].iter().all(|o| o.abs() < 1e-3),
            "probe {cell:?} is traced, active and on its lattice point: {offset:?}"
        );
        held += luminance(rb.irradiance(slot, 3, 5)) / cells.len() as f32;
    }
    // The form factor of the slit's upper opening (the rectangle |x| < 0.2, |z| < 3 at the roof's top, which
    // bounds every ray through the channel from a point under its middle line) for an up-facing point `z0`
    // along the line: (1 / pi) * integral of h^2 / (x^2 + z^2 + h^2)^2 over the rectangle.
    let h = roof + 2.0 * t - 1.0;
    let form = |z0: f32| {
        let (nx, nz) = (64, 960);
        let mut sum = 0.0f64;
        for i in 0..nx {
            let x = f64::from(-half_slit + (i as f32 + 0.5) / nx as f32 * 2.0 * half_slit);
            for j in 0..nz {
                let z = f64::from(-half_len + (j as f32 + 0.5) / nz as f32 * 2.0 * half_len - z0);
                let r2 = x * x + z * z + f64::from(h * h);
                sum += f64::from(h * h) / (r2 * r2);
            }
        }
        let cell = f64::from(2.0 * half_slit / nx as f32) * f64::from(2.0 * half_len / nz as f32);
        (sum * cell / std::f64::consts::PI) as f32
    };
    let expected = sky * (form(-1.0) + form(0.0) + form(1.0)) / 3.0;
    eprintln!(
        "a metre under a 0.4 m roof slit: the probes hold {held:.1} toward +y, the slit's form factor times the \
         sky {expected:.1} ({:.3} x)",
        held / expected
    );
    assert!(
        held > 0.96 * expected && held < 1.05 * expected,
        "the probes under the slit hold {held}, the slit lets in {expected}"
    );
}

/// ADR-5552: a probe's tile holds the light at the crease points of the surfaces around it. Two walls meet at
/// a corner on a dark ground: wall B (0.8 m wide from the corner) faces the sun, wall A lies along the sun's
/// path and gets none. The probe half a metre before both holds, for wall A, more light at the crease point 8 cm
/// from wall B than at the surface point half a metre from it (the sunlit strip fills that much more of the
/// point's view: 1.09 to 1.13 times on an RTX 5080 and on lavapipe), and toward the side with no surface within
/// reach the crease's texel is the face's own. Before, the border texels were copies of other lobes.
#[test]
fn a_crease_beside_a_sunlit_wall_holds_more_light_than_the_wall_before_it() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let t = 0.1;
    let meshes = vec![
        // A dark ground: the walls' light is the sun's and the sky's.
        block([0.0, -0.25, 0.0], [30.0, 0.25, 30.0], 0),
        // Wall A: its inner face at x = -2.5, between the probe planes at -3 and -2.
        block([-2.5 - t, 1.5, 0.0], [t, 1.5, 2.5 + 2.0 * t], 1),
        // Wall B: its inner face at z = -2.5, from the corner to x = -1.7.
        block([-2.1, 1.5, -2.5 - t], [0.4, 1.5, t], 1),
    ];
    let mut lab = scene(
        [0.5, 1.0, -1.5],
        [-2.5, 1.0, -1.5],
        meshes,
        vec![grey(0.03), grey(0.6)],
    );
    // The sun travels along wall A's face: it lights wall B and grazes wall A.
    lab.directional = DirectionalLightUniform::new([0.0, -0.819, -0.574], [1.0; 3], 100_000.0);
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(test_settings());
    renderer.set_ddgi_enabled(Some(true));
    let pixels = converge(&mut renderer, &device, &lab, 128);
    dump("crease_corner", &pixels);
    let rb = renderer
        .ddgi_read_back(&device)
        .unwrap_or_else(|err| panic!("read back: {err}"))
        .unwrap_or_else(|| panic!("the probes ran"));
    let cell = [-2, 1, -2];
    let slot = rb.slot(0, cell);
    let (offset, tag) = rb.probe(slot);
    assert!(
        tag[..3] == cell && offset[3] > 0.5 && offset[..3].iter().all(|o| o.abs() < 1e-3),
        "probe {cell:?} is traced, active and on its lattice point: {offset:?}"
    );
    // The +x face (wall A's normal): its texel at the surface (interior (5, 2)), its crease toward -z (wall
    // B: side 1) and toward +z (no surface within reach: side 0).
    let face = luminance(rb.irradiance(slot, 5, 2));
    let toward_b = luminance(rb.crease_irradiance(slot, 0, 1));
    let open = luminance(rb.crease_irradiance(slot, 0, 0));
    eprintln!(
        "wall A before the corner: {face:.1} at the surface point, {toward_b:.1} at the crease toward the sunlit \
         wall ({:.3} x), {open:.1} toward the open side ({:.4} x)",
        toward_b / face,
        open / face
    );
    assert!(face > 0.0, "the probe holds light");
    assert!(
        toward_b > 1.05 * face && toward_b < 3.0 * face,
        "the crease beside the sunlit wall holds {toward_b}, the surface point half a metre from it {face}"
    );
    assert!(
        (open / face - 1.0).abs() < 1e-3,
        "a crease with no surface beside it is the face's own light: {open} / {face}"
    );
}

/// The corner of [`a_crease_beside_a_sunlit_wall_holds_more_light_than_the_wall_before_it`] on a ground of
/// albedo `ground`: wall A (inner face at x = -2.5) along the sun's path, wall B (inner face at z = -2.5, from
/// the corner to x = -1.7) facing the sun, 100 klx at 35 degrees to B's normal.
fn crease_corner(ground: f32) -> LabScene {
    let t = 0.1;
    let meshes = vec![
        block([0.0, -0.25, 0.0], [30.0, 0.25, 30.0], 0),
        block([-2.5 - t, 1.5, 0.0], [t, 1.5, 2.5 + 2.0 * t], 1),
        block([-2.1, 1.5, -2.5 - t], [0.4, 1.5, t], 1),
    ];
    let mut lab = scene(
        [0.5, 1.0, -1.5],
        [-2.5, 1.0, -1.5],
        meshes,
        vec![grey(ground), grey(0.6)],
    );
    lab.directional = DirectionalLightUniform::new([0.0, -0.819, -0.574], [1.0; 3], 100_000.0);
    lab
}

/// ADR-5716: a crease seen from across the cell holds the surface beside it by the share of the view that
/// surface fills. In the corner of the test above, the probe stands half a metre before wall A and half a
/// metre from the sunlit wall B, and A's crease point stands 8 cm from B. B, 0.8 m wide along the shared
/// edge's normal, fills 0.45 of the crease point's cosine-weighted view and 0.235 of the surface point's (the
/// form factor from a point at clearance d to a perpendicular strip of width w: (1 - d / sqrt(d² + w²)) / 2,
/// Cohen and Wallace 1993, ch. 4); the rest of both views is the same far field, which the surface point's
/// own light gives once B's share is taken out. The crease texel holds that sum within 12 %. With the crease
/// point's rays in one weighted mean the few rays that end on B's strip counted for a fraction of B's share:
/// the texel read 0.83 of the sum (1.11 times the surface point, where the two strata read 1.39 times).
#[test]
fn a_crease_across_the_cell_holds_half_of_the_sunlit_wall_beside_it() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let lab = crease_corner(0.03);
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(test_settings());
    renderer.set_ddgi_enabled(Some(true));
    converge(&mut renderer, &device, &lab, 128);
    let rb = renderer
        .ddgi_read_back(&device)
        .unwrap_or_else(|err| panic!("read back: {err}"))
        .unwrap_or_else(|| panic!("the probes ran"));
    let cell = [-2, 1, -2];
    let slot = rb.slot(0, cell);
    let (offset, tag) = rb.probe(slot);
    assert!(
        tag[..3] == cell && offset[3] > 0.5,
        "probe {cell:?} is traced and active"
    );
    // Wall B's sunlit radiance over pi is what a probe ray reads on it: albedo 0.6 times the sun's 100 klx
    // at 0.574 to the wall's normal, over pi (its own bounce light aside).
    let sunlit = 0.6 * 100_000.0 * 0.574 / std::f32::consts::PI;
    let face = luminance(rb.irradiance(slot, 5, 2));
    let toward_b = luminance(rb.crease_irradiance(slot, 0, 1));
    // B's share of the view from a point `d` metres from it, and what the crease point should hold.
    let share = |d: f32| 0.5 * (1.0 - d / (d * d + 0.8 * 0.8).sqrt());
    let (at_crease, at_surface) = (share(0.08), share(0.5));
    let far_field = (face - at_surface * sunlit) / (1.0 - at_surface);
    let expected = at_crease * sunlit + (1.0 - at_crease) * far_field;
    eprintln!(
        "wall A's crease toward the sunlit wall B: {toward_b:.0}, {:.3} of the {expected:.0} that B's share {at_crease:.3} of its sunlit radiance {sunlit:.0} and the far field {far_field:.0} sum to (the surface point holds {face:.0}, {:.3} x)",
        toward_b / expected,
        toward_b / face
    );
    assert!(
        far_field > 0.0 && far_field < sunlit,
        "the far field is the sky and the dark ground: {far_field}"
    );
    assert!(
        (toward_b / expected - 1.0).abs() < 0.12,
        "the crease holds {toward_b}; the sunlit wall's share and the far field sum to {expected}"
    );
}

/// ADR-5716: a crease belongs to the probe row nearest its surface. Over a sunlit ground on a probe plane
/// the probe a metre up holds, for the wall before it, no crease toward the ground (its texel is the face's
/// own light): the row standing on the ground holds that crease, and a wall point between the two rows
/// already takes the gradient toward the ground from the blend between them. Before, both rows held it.
#[test]
fn the_row_above_a_floor_leaves_the_walls_crease_to_the_row_standing_on_it() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    // A bright ground: the wall's foot takes its sun bounce.
    let lab = crease_corner(0.6);
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(test_settings());
    renderer.set_ddgi_enabled(Some(true));
    converge(&mut renderer, &device, &lab, 128);
    let rb = renderer
        .ddgi_read_back(&device)
        .unwrap_or_else(|err| panic!("read back: {err}"))
        .unwrap_or_else(|| panic!("the probes ran"));
    // The probe column half a metre before wall A, in the open (two metres from wall B).
    let (upper, lower) = (rb.slot(0, [-2, 1, 0]), rb.slot(0, [-2, 0, 0]));
    for (name, slot) in [("upper", upper), ("lower", lower)] {
        let (offset, _) = rb.probe(slot);
        assert!(offset[3] > 0.5, "the {name} probe is active: {offset:?}");
    }
    // The +x face (wall A's normal) at the surface, and its crease toward -y (the ground: side 3).
    let ratio = |slot: u32| {
        let face = luminance(rb.irradiance(slot, 5, 2));
        let down = luminance(rb.crease_irradiance(slot, 0, 3));
        (face, down, down / face.max(1e-6))
    };
    let (upper_face, upper_down, upper_ratio) = ratio(upper);
    let (lower_face, lower_down, lower_ratio) = ratio(lower);
    eprintln!(
        "wall A's crease toward the ground: the probe a metre up {upper_down:.0} / {upper_face:.0} = {upper_ratio:.4}, the probe standing on the ground {lower_down:.0} / {lower_face:.0} = {lower_ratio:.4}"
    );
    assert!(upper_face > 0.0 && lower_face > 0.0, "both probes hold light");
    assert!(
        (upper_ratio - 1.0).abs() < 1e-3,
        "the row above the ground holds no crease toward it: {upper_down} / {upper_face}"
    );
    assert!(
        (lower_ratio - 1.0).abs() > 0.02,
        "the row standing on the ground holds the wall's crease toward it: {lower_down} / {lower_face}"
    );
}

/// A small emissive block (0.3 m, under `min_proxy_radius`, so it has no brick) lights the
/// floor around it through the probes' emitter list. Inside a sealed room it stays inside: the
/// probes outside see the walls in front of it in their distance moments.
#[test]
fn a_small_emitter_lights_the_probes_and_stays_inside_a_sealed_room() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let emissive = LabMaterial {
        base_color: [0.1, 0.1, 0.1, 1.0],
        emissive: [1.0, 0.9, 0.7],
        emissive_intensity: 4000.0,
        ..LabMaterial::default()
    };
    let lamp = block([0.0, 1.2, 0.0], [0.15, 0.15, 0.15], 1);
    let dark = |mut lab: LabScene| {
        lab.directional = sun(50.0, 0.0);
        lab.ambient_lux = 0.0;
        lab.ibl = false;
        lab.exposure_ev = 9.0;
        if let Some(post) = lab.post.as_mut() {
            post.exposure_ev = 9.0;
        }
        lab
    };
    let floor_luma = |emitters: bool, lab: &LabScene, rect: (u32, u32, u32, u32)| {
        let mut renderer = FrameRenderer::new();
        // The default 1 m proxy radius: the 0.3 m lamp gets no brick, so without the emitter
        // list nothing carries its light.
        renderer.set_ddgi_settings(DdgiSettings {
            emitters,
            min_proxy_radius: 1.0,
            ..test_settings()
        });
        renderer.set_ddgi_enabled(Some(true));
        // The probes alone: no area lights fitted to the lamp (`emissive_lights`).
        let mut q = quality();
        q.emissive_lights = 0;
        let px = converge_with(&mut renderer, &device, lab, 40, q);
        linear_luma(&px, rect.0, rect.1, rect.2, rect.3)
    };
    // Open floor, the lamp out of view above the camera's target.
    let mut meshes = vec![block([0.0, -0.25, 0.0], [30.0, 0.25, 30.0], 0)];
    meshes.push(lamp.clone());
    let open = dark(scene(
        [0.0, 2.0, 5.0],
        [0.0, 0.0, 1.5],
        meshes,
        vec![grey(0.6), emissive.clone()],
    ));
    // The floor around the lamp's foot (the lamp is above the top of this rectangle).
    let lit = floor_luma(true, &open, (50, 30, 110, 60));
    let unlit = floor_luma(false, &open, (50, 30, 110, 60));
    eprintln!("floor near a small emitter: emitter list {lit:.5}, rays only {unlit:.5}");
    assert!(
        lit > 4.0 * unlit.max(1e-4),
        "the emitter must light the floor ({lit} vs {unlit})"
    );
    // The same lamp inside the sealed room: the ground outside stays dark.
    let mut room = sealed_room();
    room.push(lamp);
    let outside = dark(scene(
        [10.0, 2.5, 10.0],
        [6.0, 0.0, 6.0],
        room,
        vec![grey(0.6), emissive],
    ));
    let with_lamp = floor_luma(true, &outside, (40, 50, 120, 90));
    let without = floor_luma(false, &outside, (40, 50, 120, 90));
    let leak = with_lamp - without;
    eprintln!(
        "ground outside the sealed room: lamp inside {with_lamp:.6}, emitter list off {without:.6} (open floor {lit:.5})"
    );
    assert!(
        leak <= 0.01 * lit,
        "the emitter leaks out of the sealed room ({leak} vs {lit})"
    );
}

/// Under GI Depth an emitter the short-range emissive pass fitted to area lights reaches the
/// probes only beyond the lights' range: Depth with the lights on reads the floor by the lamp
/// like Balanced with the lights on, instead of the lights plus the probes' full emitter light.
#[test]
fn a_fitted_emitter_is_counted_once_under_depth() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let emissive = LabMaterial {
        base_color: [0.1, 0.1, 0.1, 1.0],
        emissive: [1.0, 0.9, 0.7],
        emissive_intensity: 4000.0,
        ..LabMaterial::default()
    };
    let mut lab = scene(
        [0.0, 2.0, 5.0],
        [0.0, 0.0, 1.5],
        vec![
            block([0.0, -0.25, 0.0], [30.0, 0.25, 30.0], 0),
            block([0.0, 1.2, 0.0], [0.15, 0.15, 0.15], 1),
        ],
        vec![grey(0.6), emissive],
    );
    lab.directional = sun(50.0, 0.0);
    lab.ambient_lux = 0.0;
    lab.ibl = false;
    lab.exposure_ev = 9.0;
    if let Some(post) = lab.post.as_mut() {
        post.exposure_ev = 9.0;
    }
    let floor = |probes: bool, lights: bool| {
        let mut renderer = FrameRenderer::new();
        renderer.set_ddgi_settings(DdgiSettings {
            min_proxy_radius: 1.0,
            ..test_settings()
        });
        renderer.set_ddgi_enabled(Some(probes));
        let mut q = quality();
        if !lights {
            q.emissive_lights = 0;
        }
        let px = converge_with(&mut renderer, &device, &lab, 40, q);
        linear_luma(&px, 50, 30, 110, 60)
    };
    let probes_only = floor(true, false);
    let lights_only = floor(false, true);
    let both = floor(true, true);
    eprintln!(
        "floor by a fitted lamp: probes only {probes_only:.5}, lights only {lights_only:.5}, both {both:.5}"
    );
    assert!(
        probes_only > 1e-3 && lights_only > 1e-3,
        "both paths light the floor ({probes_only}, {lights_only})"
    );
    // Without the split the probes would add their whole emitter light on top of the lights.
    let extra = both - lights_only;
    assert!(
        extra < 0.35 * probes_only,
        "the probes count the fitted lamp again: both {both}, lights {lights_only}, probes {probes_only}"
    );
    assert!(
        both >= 0.97 * lights_only,
        "the probes must not remove the lights' own light ({both} vs {lights_only})"
    );
}

/// One offscreen frame. The probes bake bricks on the caller (`begin_offscreen`).
fn render_one(renderer: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene) {
    let frame = lab.as_frame_scene_with_quality(quality());
    renderer
        .render_offscreen(device, &frame)
        .unwrap_or_else(|err| panic!("frame: {err}"));
}

/// A 4 m × 0.2 m bar. `half_z` 0.10 and 0.11 quantize to the same 43×7×7 brick and hash apart.
fn bar(half_z: f32) -> LabScene {
    let (vertices, indices) = box_mesh([2.0, 0.1, half_z]);
    scene(
        [0.0, 2.0, 8.0],
        [0.0, 0.0, 0.0],
        vec![LabMesh {
            vertices,
            indices,
            material: 0,
            model: Mat4::IDENTITY.to_cols_array_2d(),
            ..LabMesh::unskinned()
        }],
        vec![grey(0.5)],
    )
}

#[test]
fn a_full_brick_pool_evicts_the_stale_mesh_and_an_opt_out_allocates_none() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let mut settings = test_settings();
    // One 43×7×7 brick is 8428 bytes (ADR-5338: a sample plane on each face). 12288 holds one and
    // refuses the next.
    settings.sdf_pool_bytes = 12288;
    settings.sdf_evict_frames = 2;
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(settings);
    renderer.set_ddgi_enabled(Some(true));
    let a = bar(0.10);
    let b = bar(0.11);
    let key_a = a.object_key(0).expect("mesh a");
    let key_b = b.object_key(0).expect("mesh b");
    assert_ne!(key_a, key_b);

    renderer.set_gi_proxy_disabled([key_a]);
    render_one(&mut renderer, &device, &a);
    assert!(renderer.ddgi_active(), "Desktop High runs the probes");
    assert!(!renderer.ddgi_brick_resident(key_a));
    assert!(!renderer.ddgi_brick_traced(key_a));

    renderer.set_gi_proxy_disabled(std::iter::empty::<u64>());
    render_one(&mut renderer, &device, &a);
    assert!(renderer.ddgi_brick_resident(key_a) && renderer.ddgi_brick_traced(key_a));
    render_one(&mut renderer, &device, &a);
    assert!(renderer.ddgi_brick_resident(key_a));

    render_one(&mut renderer, &device, &b);
    assert!(
        renderer.ddgi_brick_resident(key_a),
        "one undrawn update is inside the window"
    );
    assert!(!renderer.ddgi_brick_resident(key_b));
    assert!(!renderer.ddgi_brick_traced(key_b));

    render_one(&mut renderer, &device, &b);
    assert!(
        !renderer.ddgi_brick_resident(key_a),
        "the stale brick was evicted"
    );
    assert!(renderer.ddgi_brick_resident(key_b) && renderer.ddgi_brick_traced(key_b));
}

/// A probe in the core of a closed solid is buried (ADR-5457). A brick signs its distances only
/// within a band of the surface, and the core of a block thicker than two bands held unsigned
/// (positive) values: rays from a probe there stopped on the band's inner edge, a front face a voxel
/// inside the wall. The probe saw no back face, stayed active and held the black of a closed cavity,
/// which a lookup that weighted it took (the planet goldens' tower, drawn from inside: a black
/// region on its far walls). The baker now signs what the band closes off from the brick's border.
#[test]
fn a_probe_in_the_core_of_a_solid_block_is_buried() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    // A 9 m cube on the ground: 0.225 m voxels, a 0.34 m signed band, a core over 4 m deep.
    let meshes = vec![
        block([0.0, -0.25, 0.0], [30.0, 0.25, 30.0], 0),
        block([0.0, 4.5, -8.0], [4.5, 4.5, 4.5], 0),
    ];
    let lab = scene([0.0, 1.7, 2.0], [0.0, 2.0, -8.0], meshes, vec![grey(0.5)]);
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(test_settings());
    renderer.set_ddgi_enabled(Some(true));
    converge(&mut renderer, &device, &lab, 6);
    let rb = renderer
        .ddgi_read_back(&device)
        .unwrap_or_else(|err| panic!("read back: {err}"))
        .unwrap_or_else(|| panic!("the probes did not run"));
    // Cascade 1 (3 m): the probes at least 1.5 m inside every face of the cube.
    let mut checked = 0;
    for y in 1..=2 {
        for z in -3..=-2 {
            for x in -1..=1 {
                let (offset, tag) = rb.probe(rb.slot(1, [x, y, z]));
                if tag[..3] != [x, y, z] {
                    continue;
                }
                checked += 1;
                assert!(
                    offset[3] < 0.5,
                    "probe ({x}, {y}, {z}) in the cube's core is active (state {})",
                    offset[3]
                );
            }
        }
    }
    assert!(
        checked >= 6,
        "only {checked} probes in the core were in the volume"
    );
    // The probes in the open beside the cube are not: the ones 3 m over the ground, 6 m in front.
    let (offset, tag) = rb.probe(rb.slot(1, [0, 1, 1]));
    assert!(tag[..3] == [0, 1, 1] && offset[3] > 0.5, "{offset:?} {tag:?}");
}

/// A floor that lies on a probe plane takes the probes' light on a renderer's first frame, in every
/// texel of the screen lookup (ADR-5457). With the probes weighted at the surface point (the tiers'
/// lookup, ADR-5374) the rounding of the floor's position, rebuilt from the depth buffer, chose
/// between the probe cell over it and the one under it. A probe that stands on a surface is not
/// active until its first relocation, and the probes under a ground quad are buried: on the first
/// frame a texel that took the cell under the floor found no probe at all and fell back to the sky
/// term (the planet goldens' ground, in the one frame they capture), and it took the probes' light
/// from the second frame on. The floor here is a tenth of a millimetre under the plane, where that
/// rounding puts it.
#[test]
fn a_floor_on_a_probe_plane_takes_the_probes_light_on_the_first_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    // A ground quad (one-sided, no thickness) just under y = 0, a probe plane of every cascade; no
    // sun, a uniform sky.
    let y = -1.0e-4;
    let corner =
        |x: f32, z: f32| regia_render::LabVertex::from_pos_normal_uv([x, y, z], [0.0, 1.0, 0.0], [0.0; 2]);
    let meshes = vec![LabMesh {
        vertices: vec![
            corner(-200.0, -200.0),
            corner(200.0, -200.0),
            corner(200.0, 200.0),
            corner(-200.0, 200.0),
        ],
        indices: vec![0, 2, 1, 0, 3, 2],
        material: 0,
        ..LabMesh::unskinned()
    }];
    let mut lab = scene([0.0, 1.7, 0.0], [0.0, 0.5, -6.0], meshes, vec![grey(0.5)]);
    lab.directional.illuminance = 0.0;
    let sky = lab.ambient_lux / std::f32::consts::PI;
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(DdgiSettings {
        visibility_blend: true,
        ..test_settings()
    });
    renderer.set_ddgi_enabled(Some(true));
    render_one(&mut renderer, &device, &lab);
    let rb = renderer
        .ddgi_read_back(&device)
        .unwrap_or_else(|err| panic!("read back: {err}"))
        .unwrap_or_else(|| panic!("the probes did not run"));
    let (w, h, texels) = rb
        .screen_irradiance()
        .unwrap_or_else(|| panic!("the screen lookup did not run"));
    // The lower third of the view is floor within the 1 m cascade.
    let (mut floor, mut dark, mut uncovered) = (0, 0, 0);
    for y in h * 2 / 3..h {
        for x in 0..w {
            let t = texels[(y * w + x) as usize];
            floor += 1;
            if t.coverage < 0.99 {
                uncovered += 1;
            }
            // Toward +y the floor sees the whole sky: irradiance / π is the sky's radiance.
            let up = 0.2126 * t.e[2][0] + 0.7152 * t.e[2][1] + 0.0722 * t.e[2][2];
            if up < 0.8 * sky {
                dark += 1;
            }
        }
    }
    eprintln!(
        "first frame: {floor} floor texels, {uncovered} without probe light, {dark} under 80 % of the sky"
    );
    assert!(floor > 500);
    assert_eq!(uncovered, 0, "{uncovered} of {floor} floor texels took no probe");
    assert_eq!(
        dark, 0,
        "{dark} of {floor} floor texels read under 80 % of the sky"
    );
}

/// The window of [`window_room`]: half its width, its sill and its head, metres.
const WINDOW_HALF: f32 = 0.8;
const WINDOW_SILL: f32 = 0.2;
const WINDOW_HEAD: f32 = 1.8;
/// The window wall's inner and outer face of [`window_room`] (z), metres.
const WINDOW_Z: [f32; 2] = [2.5, 2.7];
/// The back wall's face of [`window_room`] (z), metres: 0.9 m behind the probe plane at z = 1.
const WINDOW_BACK_Z: f32 = 0.1;

/// A room lit by the sky through one window (ADR-5909): 5 m wide in x and 3 m tall, 2.4 m deep from its back wall
/// to its window wall (20 cm walls, floor and roof), a window 1.6 m square in the window wall before the probe at
/// (0, 1, 1). Nothing lies outside it (the flat sky all round, under the floor too), no sun shines, and every
/// surface reflects 2 %, so the light on a wall is the window's.
fn window_room() -> LabScene {
    let t = 0.1;
    let (hx, h) = (2.5f32, 3.0f32);
    let zc = 0.5 * (WINDOW_BACK_Z + WINDOW_Z[0]);
    let hz = 0.5 * (WINDOW_Z[0] - WINDOW_BACK_Z);
    let beside = 0.5 * (hx - WINDOW_HALF);
    let meshes = vec![
        // Floor and roof.
        block([0.0, -t, zc], [hx + 2.0 * t, t, hz + 2.0 * t], 0),
        block([0.0, h + t, zc], [hx + 2.0 * t, t, hz + 2.0 * t], 0),
        // Side walls and the back wall.
        block([hx + t, h / 2.0, zc], [t, h / 2.0, hz + 2.0 * t], 0),
        block([-hx - t, h / 2.0, zc], [t, h / 2.0, hz + 2.0 * t], 0),
        block([0.0, h / 2.0, WINDOW_BACK_Z - t], [hx, h / 2.0, t], 0),
        // The window wall: beside the window, under its sill and over its head.
        block(
            [-WINDOW_HALF - beside, h / 2.0, WINDOW_Z[0] + t],
            [beside, h / 2.0, t],
            0,
        ),
        block(
            [WINDOW_HALF + beside, h / 2.0, WINDOW_Z[0] + t],
            [beside, h / 2.0, t],
            0,
        ),
        block(
            [0.0, WINDOW_SILL / 2.0, WINDOW_Z[0] + t],
            [WINDOW_HALF, WINDOW_SILL / 2.0, t],
            0,
        ),
        block(
            [0.0, (WINDOW_HEAD + h) / 2.0, WINDOW_Z[0] + t],
            [WINDOW_HALF, (h - WINDOW_HEAD) / 2.0, t],
            0,
        ),
    ];
    let mut lab = scene(
        [0.0, 1.5, 1.3],
        [0.0, 1.0, WINDOW_BACK_Z],
        meshes,
        vec![grey(0.02)],
    );
    lab.directional = DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0);
    lab
}

/// The cosine-weighted solid angle of [`window_room`]'s window from `q` for a surface that faces +z: every
/// direction that passes the opening at both faces of the window wall, each by its cosine at `q` (a midpoint rule
/// of 96 × 96 over the opening's inner face).
fn window_view(q: [f32; 3]) -> f32 {
    let n = 96u32;
    let (w, h) = (2.0 * WINDOW_HALF, WINDOW_HEAD - WINDOW_SILL);
    let mut sum = 0.0f64;
    for iy in 0..n {
        for ix in 0..n {
            let x = -WINDOW_HALF + (ix as f32 + 0.5) * w / n as f32;
            let y = WINDOW_SILL + (iy as f32 + 0.5) * h / n as f32;
            let d = [x - q[0], y - q[1], WINDOW_Z[0] - q[2]];
            // Where the direction leaves the wall's outer face.
            let k = (WINDOW_Z[1] - q[2]) / d[2];
            let (ox, oy) = (q[0] + d[0] * k, q[1] + d[1] * k);
            if ox.abs() > WINDOW_HALF || !(WINDOW_SILL..=WINDOW_HEAD).contains(&oy) {
                continue;
            }
            // The cosine at `q` times the cosine at the opening, over the squared distance.
            let r2 = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
            sum += f64::from(d[2] * d[2] / (r2 * r2));
        }
    }
    (sum * f64::from(w * h) / f64::from(n * n)) as f32
}

/// ADR-5909: light that reached a probe through an opening is weighed for the probe's surface texels from the
/// opening, not from what lies behind it. In a room lit by the sky through one window, with walls that reflect
/// 2 %, the light on the back wall is the window's, and a point of that wall 0.9 m behind a probe sees the window
/// under 0.47 of the cosine-weighted solid angle the probe sees it under ([`window_view`]: the opening 2.4 m away
/// against 1.5 m). The probe's surface texel for that wall holds that share of the probe's own lobe. Before, a ray
/// through the window was weighed by its direction alone, which is how the probe saw it: the texel held the
/// probe's own light (1.00 of it), the light 0.9 m nearer the window than the wall stands.
#[test]
fn a_wall_behind_a_probe_takes_a_windows_light_as_the_wall_sees_the_window() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let lab = window_room();
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(test_settings());
    renderer.set_ddgi_enabled(Some(true));
    converge(&mut renderer, &device, &lab, 160);
    // The probe's +z lobe at the probe (2, 2) and at the back wall behind it (3, 2), and its -z lobe (0, 0), summed
    // over four read-backs 16 frames apart: only a few of a probe's 64 rays pass the window on each update.
    let cell = [0, 1, 1];
    let (mut at_probe, mut at_wall, mut away) = (0.0f32, 0.0f32, 0.0f32);
    for _ in 0..4 {
        converge(&mut renderer, &device, &lab, 16);
        let rb = renderer
            .ddgi_read_back(&device)
            .unwrap_or_else(|err| panic!("read back: {err}"))
            .unwrap_or_else(|| panic!("the probes ran"));
        let slot = rb.slot(0, cell);
        let (offset, tag) = rb.probe(slot);
        assert!(
            tag[..3] == cell && offset[3] > 0.5,
            "probe {cell:?} is traced and active"
        );
        at_probe += luminance(rb.irradiance(slot, 2, 2));
        at_wall += luminance(rb.irradiance(slot, 3, 2));
        away += luminance(rb.irradiance(slot, 0, 0));
    }
    let expected = window_view([0.0, 1.0, WINDOW_BACK_Z]) / window_view([0.0, 1.0, 1.0]);
    eprintln!(
        "window room, the probe 0.9 m before the back wall: the wall's texel holds {:.3} of the probe's own lobe \
         ({at_wall:.1} / {at_probe:.1}); the window's view from the wall is {expected:.3} of its view from the \
         probe; the lobe away from the window holds {:.4}",
        at_wall / at_probe,
        away / at_probe
    );
    assert!(
        at_probe > 0.0 && away < 0.1 * at_probe,
        "the room's light is the window's: {at_probe} toward it, {away} away from it"
    );
    assert!(
        (0.4..0.55).contains(&expected),
        "the window's view from the wall over its view from the probe: {expected}"
    );
    assert!(
        at_wall > 0.75 * expected * at_probe && at_wall < 1.5 * expected * at_probe,
        "the back wall's texel holds {:.3} of the probe's lobe; the wall sees the window under {expected:.3} of \
         the probe's view of it",
        at_wall / at_probe
    );
}
