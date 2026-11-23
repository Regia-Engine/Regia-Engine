// SPDX-License-Identifier: MPL-2.0
//! `ProbeSpecular::LobeHue` against the path tracer (ADR-2996): the probe-lit indirect specular of
//! metal spheres, default and mode, scored against the tracer's `indirect_specular` AOV on the two
//! scenes the ADR's table quotes, through traced probes and a baked volume both:
//!
//! - **Studio**: a mirror and a satin sphere under the blue-heavy gradient of `studio_gradient.rs`,
//!   the image's own ground, then an analytic ground plane of a colored albedo (`set_environment_ground`,
//!   a diffuse slab in the tracer's scene).
//! - **Interior**: a room with a red and a green wall, lit by the same sky through a roof opening.
//!
//! Hand-run, for the ADR and the owner's look call: the mode's default is the owner's decision.
//!
//! ```text
//! REGIA_PROBE_HUE_PLATES=<dir> cargo test --release -p regia_pt --test probe_specular_hue -- --ignored --nocapture
//! ```
//!
//! It asserts only that a score was measured on real pixels; the numbers go in the ADR. Plates, one
//! PNG a case: tracer | default | mode, through the runtime's display transform.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use regia_asset::baked_gi::{BakedGiVolume, TerrainLayer, VolumeLayout};
use regia_core::glam::{Mat4, Vec3};
use regia_pt::env_map::EnvMap;
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::probe::{GiBakeSettings, ProbeTraceSettings, bake_gi_volume};
use regia_pt::{PathTracer, PtSettings};
use regia_render::compare::{DEFAULT_PPD, compare_linear, display_encode};
use regia_render::{
    BakedGiSettings, DdgiSettings, DirectionalLightUniform, EnvGroundPlane, EnvironmentAuthority,
    FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, PostSettings, ProbeSpecular, RenderDevice,
    RenderProfile, RenderProfilePreset, SsaoMode, StudioGradient, box_mesh, translate_mesh, uv_sphere,
};

const W: u32 = 320;
const H: u32 = 160;
const EV: f32 = 13.0;
const SPP: u32 = 1024;

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| RenderDevice::headless().expect("the comparison needs a GPU; lavapipe is a GPU here"))
}

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| PathTracer::new(device()).expect("compile the path tracer"))
}

fn gradient() -> StudioGradient {
    StudioGradient::new(
        [2400.0, 4200.0, 8000.0],
        [2600.0, 2800.0, 3000.0],
        [600.0, 600.0, 600.0],
    )
}

fn sky_options() -> ExtractOptions {
    let g = gradient();
    let map = EnvMap::from_fn(512, 256, |d| g.radiance(d));
    ExtractOptions {
        sky: Some(map.into_sky(0.0, 1.0).expect("the gradient's map")),
        ..ExtractOptions::default()
    }
}

fn material(rgb: [f32; 3], metallic: f32, roughness: f32) -> LabMaterial {
    LabMaterial {
        base_color: [rgb[0], rgb[1], rgb[2], 1.0],
        metallic,
        roughness,
        ..LabMaterial::default()
    }
}

fn sphere(at: [f32; 3], radius: f32, material: usize) -> LabMesh {
    let (v, i) = uv_sphere(radius, 128, 64);
    LabMesh {
        vertices: translate_mesh(&v, at),
        indices: i,
        material,
        ..LabMesh::unskinned()
    }
}

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

fn quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.ssao = SsaoMode::Off;
    q.taau = false;
    q.screen_space_gi = false;
    q.ssr = false;
    q.reflection_probes = false;
    q.render_scale_permille = 1000;
    q
}

fn base_scene(
    eye: [f32; 3],
    target: [f32; 3],
    meshes: Vec<LabMesh>,
    materials: Vec<LabMaterial>,
) -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye,
        target,
        fov_y: 0.7,
        near: 0.05,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: EV,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            auto_exposure: false,
            exposure_ev: EV,
            bloom: false,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials,
        meshes,
    }
}

/// A sphere the score reads: center, radius.
type Target = ([f32; 3], f32);

const STUDIO_SPHERES: [Target; 2] = [([-1.75, 1.3, 0.0], 1.3), ([1.75, 1.3, 0.0], 1.3)];
const PLANE_ALBEDO: [f32; 3] = [0.75, 0.32, 0.08];

fn studio(with_plane_mesh: bool) -> LabScene {
    let mut meshes = vec![
        sphere(STUDIO_SPHERES[0].0, STUDIO_SPHERES[0].1, 0),
        sphere(STUDIO_SPHERES[1].0, STUDIO_SPHERES[1].1, 1),
    ];
    if with_plane_mesh {
        meshes.push(block([0.0, -0.5, 0.0], [200.0, 0.5, 200.0], 2));
    }
    base_scene(
        [0.0, 1.4, 8.5],
        [0.0, 1.3, 0.0],
        meshes,
        vec![
            material([1.0; 3], 1.0, 0.05),
            material([1.0; 3], 1.0, 0.35),
            material(PLANE_ALBEDO, 0.0, 1.0),
        ],
    )
}

const ROOM_SPHERES: [Target; 2] = [([-1.5, 1.1, 0.0], 1.1), ([1.5, 1.1, 0.0], 1.1)];

fn interior() -> LabScene {
    let meshes = vec![
        sphere(ROOM_SPHERES[0].0, ROOM_SPHERES[0].1, 0),
        sphere(ROOM_SPHERES[1].0, ROOM_SPHERES[1].1, 1),
        block([0.0, -0.1, 0.0], [5.2, 0.1, 5.2], 2),
        block([-5.1, 2.0, 0.0], [0.1, 2.0, 5.2], 3),
        block([5.1, 2.0, 0.0], [0.1, 2.0, 5.2], 4),
        block([0.0, 2.0, -5.1], [5.2, 2.0, 0.1], 2),
        block([0.0, 2.0, 5.1], [5.2, 2.0, 0.1], 2),
        // The roof, with a 4 × 4 m opening over the middle of the room.
        block([0.0, 4.1, -3.6], [5.2, 0.1, 1.6], 2),
        block([0.0, 4.1, 3.6], [5.2, 0.1, 1.6], 2),
        block([-3.6, 4.1, 0.0], [1.6, 0.1, 2.0], 2),
        block([3.6, 4.1, 0.0], [1.6, 0.1, 2.0], 2),
    ];
    base_scene(
        [0.0, 1.5, 4.8],
        [0.0, 1.2, 0.0],
        meshes,
        vec![
            material([1.0; 3], 1.0, 0.05),
            material([1.0; 3], 1.0, 0.35),
            material([0.75; 3], 0.0, 0.95),
            material([0.75, 0.08, 0.06], 0.0, 0.95),
            material([0.08, 0.7, 0.1], 0.0, 0.95),
        ],
    )
}

/// Pixels whose camera ray hits one of `spheres` (shrunk one pixel off the silhouette).
fn mask_of(scene: &LabScene, spheres: &[Target]) -> Vec<bool> {
    let eye = Vec3::from_array(scene.eye);
    let view = regia_core::glam::camera::rh::view::look_at_mat4(eye, Vec3::from_array(scene.target), Vec3::Y);
    let proj =
        regia_core::glam::camera::rh::proj::directx::perspective(scene.fov_y, W as f32 / H as f32, 0.1, 40.0);
    let inv = (proj * view).inverse();
    let hits = |x: f32, y: f32| {
        let ndc = Vec3::new(x / W as f32 * 2.0 - 1.0, 1.0 - y / H as f32 * 2.0, 1.0);
        let dir = (inv.project_point3(ndc) - eye).normalize();
        spheres.iter().any(|(center, radius)| {
            let oc = eye - Vec3::from_array(*center);
            let b = oc.dot(dir);
            let r = radius * 0.97;
            b * b - (oc.length_squared() - r * r) > 0.0
        })
    };
    (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .map(|(x, y)| hits(x as f32 + 0.5, y as f32 + 0.5))
        .collect()
}

/// A terrain layer of probes over `scene`, baked by the tracer.
fn bake(
    scene: &LabScene,
    origin: [f32; 2],
    spacing: f32,
    counts: [u32; 2],
    levels: Vec<f32>,
    ground: f32,
) -> Arc<BakedGiVolume> {
    let pt = from_frame(&scene.as_frame_scene_with_quality(quality()), &sky_options());
    let mut layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin,
            spacing: [spacing, spacing],
            counts,
            levels,
            ground: vec![ground; (counts[0] * counts[1]) as usize],
        }),
        bricks: Vec::new(),
        moment_resolution: 8,
        max_distance: 32.0,
    };
    layout.assign_brick_offsets();
    let settings = GiBakeSettings {
        trace: ProbeTraceSettings {
            rays_per_probe: 256,
            samples: 8,
            max_bounces: 3,
            seed: 11,
            samples_per_submit: 4,
            ..ProbeTraceSettings::default()
        },
        ..GiBakeSettings::default()
    };
    let baked = bake_gi_volume(tracer(), &pt, &layout, &settings, |_, _, _| true).expect("bake");
    Arc::new(baked.volume)
}

/// Which probes light the frame, and how.
#[derive(Clone)]
enum Probes {
    Traced,
    Baked { volume: Arc<BakedGiVolume>, zh3: bool },
}

fn realtime(
    scene: &LabScene,
    probes: &Probes,
    mode: ProbeSpecular,
    plane: Option<EnvGroundPlane>,
) -> Vec<[f32; 3]> {
    let device = device();
    let mut q = quality();
    let mut renderer = FrameRenderer::new();
    renderer.set_environment_authority(EnvironmentAuthority::Studio);
    renderer
        .set_studio_gradient(device, Some(&gradient()))
        .expect("gradient");
    renderer.set_environment_ground(plane).expect("plane");
    let count = match probes {
        Probes::Traced => {
            renderer.set_ddgi_settings(DdgiSettings {
                specular: mode,
                ..renderer.ddgi_settings().clone()
            });
            48
        }
        Probes::Baked { volume, zh3 } => {
            q.gi_probes = false;
            q.baked_gi = true;
            renderer.set_baked_gi(Some(volume.clone()));
            renderer.set_baked_gi_settings(BakedGiSettings {
                specular: mode,
                zh3: *zh3,
                ..BakedGiSettings::default()
            });
            3
        }
    };
    let frame = scene.as_frame_scene_with_quality(q);
    for i in 0..count {
        if i + 1 == count {
            renderer.request_hdr_capture();
        }
        renderer.render_offscreen(device, &frame).expect("frame");
    }
    renderer
        .take_hdr_capture(device)
        .expect("readback")
        .expect("hdr")
        .rgb
}

/// The numbers of one image against the tracer over the mask.
struct Score {
    flip: f32,
    rgb_error: f32,
    chroma_deg: f32,
    chroma_far: f32,
    level: f32,
}

fn score(reference: &[[f32; 3]], test: &[[f32; 3]], mask: &[bool]) -> Score {
    let blended: Vec<[f32; 3]> = test
        .iter()
        .zip(reference)
        .zip(mask)
        .map(|((t, r), m)| if *m { *t } else { *r })
        .collect();
    let (_, flip) = compare_linear(reference, &blended, W, H, EV, DEFAULT_PPD);
    let mut flip_sum = 0.0f64;
    let (mut err, mut total, mut angle, mut far, mut n) = (0.0f64, 0.0f64, 0.0f64, 0usize, 0usize);
    let (mut lum_t, mut lum_r) = (0.0f64, 0.0f64);
    let luma = |c: [f32; 3]| f64::from(0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]);
    for ((t, r), (m, f)) in test.iter().zip(reference).zip(mask.iter().zip(&flip.map)) {
        if !*m {
            continue;
        }
        flip_sum += f64::from(*f);
        for c in 0..3 {
            err += f64::from((t[c] - r[c]).abs());
            total += f64::from(r[c].abs());
        }
        let dot = t[0] * r[0] + t[1] * r[1] + t[2] * r[2];
        let lens = (t[0] * t[0] + t[1] * t[1] + t[2] * t[2]).sqrt()
            * (r[0] * r[0] + r[1] * r[1] + r[2] * r[2]).sqrt();
        let a = f64::from((dot / lens.max(1e-6)).clamp(-1.0, 1.0).acos().to_degrees());
        angle += a;
        far += usize::from(a > 15.0);
        lum_t += luma(*t);
        lum_r += luma(*r);
        n += 1;
    }
    assert!(n > 500, "the mask covers the spheres ({n} pixels)");
    Score {
        flip: (flip_sum / n as f64) as f32,
        rgb_error: (err / total.max(1e-6)) as f32,
        chroma_deg: (angle / n as f64) as f32,
        chroma_far: far as f32 / n as f32,
        level: (lum_t / lum_r.max(1e-6)) as f32,
    }
}

fn plates(name: &str, images: &[&[[f32; 3]]]) {
    let Some(dir) = std::env::var_os("REGIA_PROBE_HUE_PLATES").map(PathBuf::from) else {
        return;
    };
    std::fs::create_dir_all(&dir).expect("plate dir");
    let width = W * images.len() as u32;
    let encoded: Vec<Vec<[f32; 3]>> = images.iter().map(|i| display_encode(i, EV)).collect();
    let mut rgba = Vec::with_capacity((width * H * 4) as usize);
    for y in 0..H {
        for image in &encoded {
            for x in 0..W {
                let c = image[(y * W + x) as usize];
                let e = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                rgba.extend_from_slice(&[e(c[0]), e(c[1]), e(c[2]), 255]);
            }
        }
    }
    regia_pt::output::write_png(&dir.join(format!("{name}.png")), width, H, &rgba).expect("plate");
}

fn reference(scene: &LabScene) -> Vec<[f32; 3]> {
    tracer()
        .render(
            &from_frame(&scene.as_frame_scene_with_quality(quality()), &sky_options()),
            &PtSettings {
                spp: SPP,
                max_bounces: 6,
                ..PtSettings::default()
            },
        )
        .expect("trace")
        .indirect_specular
}

fn case(
    name: &str,
    scene: &LabScene,
    reference: &[[f32; 3]],
    spheres: &[Target],
    probes: &Probes,
    plane: Option<EnvGroundPlane>,
) {
    let mask = mask_of(scene, spheres);
    let off = realtime(scene, probes, ProbeSpecular::Lobe, plane);
    let on = realtime(scene, probes, ProbeSpecular::LobeHue, plane);
    plates(name, &[reference, &off, &on]);
    let (a, b) = (score(reference, &off, &mask), score(reference, &on, &mask));
    println!(
        "| {name} | {:.4} | {:.4} | {:.3} | {:.3} | {:.1} | {:.1} | {:.3} | {:.3} | {:.2} | {:.2} |",
        a.flip,
        b.flip,
        a.rgb_error,
        b.rgb_error,
        a.chroma_deg,
        b.chroma_deg,
        a.chroma_far,
        b.chroma_far,
        a.level,
        b.level
    );
}

#[test]
#[ignore = "PT reference look report for ADR-2996: see the module docs"]
fn the_mode_against_the_path_tracer() {
    println!(
        "| case | FLIP off | FLIP on | RGB err off | RGB err on | chroma deg off | on | chroma >15 deg off | on | level off | on |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|");
    let open = base_scene(
        [0.0, 1.4, 8.5],
        [0.0, 1.3, 0.0],
        Vec::new(),
        vec![material([1.0; 3], 0.0, 1.0)],
    );
    let studio_volume = bake(&open, [-12.0, -12.0], 4.0, [7, 7], vec![0.5, 2.0, 4.0, 8.0], 0.0);
    let plane = EnvGroundPlane {
        height: 0.0,
        albedo: PLANE_ALBEDO,
        sun_bounce: false,
        ..EnvGroundPlane::default()
    };
    let scene = studio(false);
    let open_reference = reference(&scene);
    let plane_reference = reference(&studio(true));
    let baked = |volume: &Arc<BakedGiVolume>, zh3| Probes::Baked {
        volume: volume.clone(),
        zh3,
    };
    let spheres = &STUDIO_SPHERES;
    case(
        "studio_traced",
        &scene,
        &open_reference,
        spheres,
        &Probes::Traced,
        None,
    );
    case(
        "studio_baked",
        &scene,
        &open_reference,
        spheres,
        &baked(&studio_volume, false),
        None,
    );
    case(
        "studio_baked_zh3",
        &scene,
        &open_reference,
        spheres,
        &baked(&studio_volume, true),
        None,
    );
    case(
        "studio_plane_traced",
        &scene,
        &plane_reference,
        spheres,
        &Probes::Traced,
        Some(plane),
    );
    case(
        "studio_plane_baked",
        &scene,
        &plane_reference,
        spheres,
        &baked(&studio_volume, false),
        Some(plane),
    );

    let room = interior();
    let room_reference = reference(&room);
    let room_volume = bake(&room, [-4.5, -4.5], 1.5, [7, 7], vec![0.4, 1.4, 2.4, 3.4], 0.0);
    let spheres = &ROOM_SPHERES;
    case(
        "interior_traced",
        &room,
        &room_reference,
        spheres,
        &Probes::Traced,
        None,
    );
    case(
        "interior_baked",
        &room,
        &room_reference,
        spheres,
        &baked(&room_volume, false),
        None,
    );
    case(
        "interior_baked_zh3",
        &room,
        &room_reference,
        spheres,
        &baked(&room_volume, true),
        None,
    );
}
