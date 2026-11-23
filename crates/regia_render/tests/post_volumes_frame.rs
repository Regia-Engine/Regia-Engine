// SPDX-License-Identifier: MPL-2.0
//! Post volumes and the physical camera on the runtime path, rendered (ADR-1464): a scene spawned
//! through `spawn_lab_scene`, extracted by `extract_frame` and rendered offscreen. Needs no golden:
//! every check compares frames of the same scene.
//!
//! - A scene without volumes and one with an unbound volume that overrides nothing render the same
//!   bytes, and so does a volume whose band ends before the camera.
//! - Walking into an "indoor" volume (+1 EV, desaturated) brightens and greys the frame
//!   monotonically: outdoors, half way through the band, inside.
//! - A physical camera at sunny 16 renders the same bytes as the camera pinned at the lens's
//!   EV100, and two stops wider is brighter.
//! - A walk from sunlight through a doorway into a room with an indoor volume (+2.5 EV, warmer,
//!   vignette, 3 m band): the view's exposure compensation rises monotonically along the path, and
//!   the room renders brighter with the volume than without it.
//!
//! Set `REGIA_POST_VOLUME_FRAMES=<dir>` to write every frame as a PNG (for a look on a real GPU:
//! `REGIA_GPU_UUID=<uuid> REGIA_GPU_STRICT=1`).

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_render::post_volumes::{PostOverrides, PostVolume, PostVolumeShape};
use regia_render::{
    CameraExposureMode, DirectionalLightUniform, ExtractedFrame, FrameRenderer, LabLocalLight, LabMaterial,
    LabMesh, LabPointLight, LabScene, LabVertex, PhysicalCamera, PostSettings, RenderDevice,
    RenderProfilePreset, RenderSettings, extract_frame, spawn_lab_scene,
};
use regia_scene::{GlobalTransform, Transform};

const W: u32 = 160;
const H: u32 = 120;
const EYE: [f32; 3] = [1.6, 2.2, 3.2];
const SUNNY_EV: f32 = 14.97;

/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A sunlit floor with a red sphere and a blue box on it, the camera pinned at `exposure_ev`.
fn scene(exposure_ev: f32) -> LabScene {
    let h = 4.0;
    let corners = [[-h, 0.0, -h], [h, 0.0, -h], [h, 0.0, h], [-h, 0.0, h]];
    let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    let floor = corners
        .into_iter()
        .zip(uvs)
        .map(|(position, uv)| LabVertex {
            position,
            normal: [0.0, 1.0, 0.0],
            uv,
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0, 1.0, 1.0, 1.0],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect();
    let (sphere, sphere_idx) = regia_render::uv_sphere(0.6, 24, 16);
    let (cube, cube_idx) = regia_render::box_mesh([0.5, 0.5, 0.5]);
    let mat = |c: [f32; 3]| LabMaterial {
        base_color: [c[0], c[1], c[2], 1.0],
        roughness: 0.6,
        ..LabMaterial::default()
    };
    LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: [0.0, 0.5, 0.0],
        fov_y: 45.0_f32.to_radians(),
        near: 0.1,
        far: 40.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev,
        ambient_lux: 10_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.35, -1.0, 0.25], [1.0, 0.98, 0.94], 100_000.0),
        local_lights: vec![],
        materials: vec![
            mat([0.7, 0.7, 0.68]),
            mat([0.75, 0.12, 0.08]),
            mat([0.1, 0.25, 0.7]),
        ],
        meshes: vec![
            LabMesh {
                vertices: floor,
                indices: vec![0, 2, 1, 0, 3, 2],
                material: 0,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: regia_render::translate_mesh(&sphere, [0.0, 0.6, 0.0]),
                indices: sphere_idx,
                material: 1,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: regia_render::translate_mesh(&cube, [-1.4, 0.5, -0.6]),
                indices: cube_idx,
                material: 2,
                ..LabMesh::unskinned()
            },
        ],
    }
}

/// What the camera entity carries besides the lab camera.
#[derive(Clone, Copy)]
enum Lens {
    None,
    Physical(PhysicalCamera),
}

/// Spawns the floor scene, adds `volumes` (each at a world position), extracts and renders it.
fn render(device: &RenderDevice, name: &str, volumes: &[(PostVolume, [f32; 3])], lens: Lens) -> Vec<u8> {
    render_at(device, name, SUNNY_EV, volumes, lens)
}

/// [`render`] with the lab camera pinned at `exposure_ev`.
fn render_at(
    device: &RenderDevice,
    name: &str,
    exposure_ev: f32,
    volumes: &[(PostVolume, [f32; 3])],
    lens: Lens,
) -> Vec<u8> {
    render_scene(device, name, &scene(exposure_ev), volumes, lens).0
}

/// Spawns `lab`, adds `volumes` and the lens, extracts and renders it. Returns the pixels and the
/// extracted view's post settings.
fn render_scene(
    device: &RenderDevice,
    name: &str,
    lab: &LabScene,
    volumes: &[(PostVolume, [f32; 3])],
    lens: Lens,
) -> (Vec<u8>, PostSettings) {
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.insert_resource(RenderSettings {
        post: PostSettings::golden_deterministic(),
        ..RenderSettings::from_profile(RenderProfilePreset::DesktopLow)
    });
    let camera = spawn_lab_scene(app.world_mut(), lab);
    if let Lens::Physical(lens) = lens {
        app.world_mut().entity_mut(camera).insert(lens);
    }
    for (volume, position) in volumes {
        let world =
            GlobalTransform(Transform::from_translation(Vec3::from_array(*position)).compute_affine());
        app.world_mut().spawn((volume.clone(), world));
    }
    app.add_systems(Render, extract_frame);
    app.update();
    let extracted = app.world().resource::<ExtractedFrame>().clone();
    let post = extracted.views[0].post.clone();
    let frame = extracted.frame_scene(0, lab.width, lab.height);
    let pixels = FrameRenderer::new()
        .render_offscreen(device, &frame)
        .expect("render")
        .pixels;
    write_frame(name, &pixels, lab.width, lab.height);
    (pixels, post)
}

/// Writes `pixels` to `$REGIA_POST_VOLUME_FRAMES/<name>.png` when the variable is set.
fn write_frame(name: &str, pixels: &[u8], width: u32, height: u32) {
    let Some(dir) = std::env::var_os("REGIA_POST_VOLUME_FRAMES") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("frame dir");
    let tmp = dir.join(format!("{name}.png.tmp"));
    {
        let file = std::fs::File::create(&tmp).expect("frame file");
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(pixels).expect("png data");
    }
    std::fs::rename(&tmp, dir.join(format!("{name}.png"))).expect("frame rename");
}

/// Mean Rec.709 luma (display bytes) and mean chroma spread (max - min channel).
fn stats(pixels: &[u8]) -> (f32, f32) {
    let (mut luma, mut spread) = (0.0f32, 0.0f32);
    let texels = pixels.as_chunks::<4>().0;
    for p in texels {
        let [r, g, b] = [f32::from(p[0]), f32::from(p[1]), f32::from(p[2])];
        luma += 0.2126 * r + 0.7152 * g + 0.0722 * b;
        spread += r.max(g).max(b) - r.min(g).min(b);
    }
    let n = texels.len().max(1) as f32;
    (luma / n, spread / n)
}

fn worst_difference(a: &[u8], b: &[u8]) -> u8 {
    a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0)
}

/// The "indoor" look: one stop brighter, nearly grey.
fn indoor(blend_distance: f32) -> PostVolume {
    PostVolume {
        shape: PostVolumeShape::Box {
            half_extents: [1.0, 1.0, 1.0],
        },
        blend_distance,
        priority: 1,
        overrides: PostOverrides {
            exposure_compensation: Some(1.0),
            saturation: Some(0.0),
            ..PostOverrides::default()
        },
        ..PostVolume::default()
    }
}

#[test]
fn volumes_change_the_frame_only_where_they_reach_and_blend_through_the_band() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)");
    let outdoor = render(&device, "outdoor", &[], Lens::None);

    let empty = PostVolume {
        shape: PostVolumeShape::Unbound,
        ..PostVolume::default()
    };
    let neutral = render(&device, "unbound_empty", &[(empty, [0.0; 3])], Lens::None);
    assert_eq!(
        worst_difference(&outdoor, &neutral),
        0,
        "an unbound volume that overrides nothing must not change a byte"
    );

    // The box's surface is 1 m from its centre; at 5 m along X with a 2 m band, the eye is 3 m
    // outside the band.
    let away = [EYE[0] + 5.0, EYE[1], EYE[2]];
    let beyond = render(&device, "beyond_band", &[(indoor(2.0), away)], Lens::None);
    assert_eq!(
        worst_difference(&outdoor, &beyond),
        0,
        "a volume out of reach changes nothing"
    );

    // 2 m from the centre is 1 m outside the surface: half way through a 2 m band (weight 0.5).
    let half = [EYE[0] + 2.0, EYE[1], EYE[2]];
    let mid = render(&device, "half_band", &[(indoor(2.0), half)], Lens::None);
    let inside = render(&device, "indoor", &[(indoor(2.0), EYE)], Lens::None);

    let (l_out, s_out) = stats(&outdoor);
    let (l_mid, s_mid) = stats(&mid);
    let (l_in, s_in) = stats(&inside);
    eprintln!(
        "luma out {l_out:.1} mid {l_mid:.1} in {l_in:.1}; spread out {s_out:.1} mid {s_mid:.1} in {s_in:.1}"
    );
    assert!(
        l_mid > l_out + 2.0,
        "half way is brighter than outdoors: {l_out} -> {l_mid}"
    );
    assert!(
        l_in > l_mid + 2.0,
        "inside is brighter than half way: {l_mid} -> {l_in}"
    );
    assert!(s_mid < s_out, "half way is less saturated: {s_out} -> {s_mid}");
    assert!(s_in < s_mid, "inside is less saturated still: {s_mid} -> {s_in}");
    assert!(s_in < 3.0, "saturation 0 prints (nearly) grey: spread {s_in}");
}

#[test]
fn a_physical_camera_exposes_from_its_lens() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)");
    let sunny16 = PhysicalCamera {
        exposure: CameraExposureMode::Physical,
        aperture_f_stops: 16.0,
        shutter_seconds: 1.0 / 125.0,
        iso: 100.0,
        ..PhysicalCamera::default()
    };
    assert!((sunny16.ev100() - SUNNY_EV).abs() < 0.01);
    // The lab camera is pinned (`Camera::exposure_ev`); pin it at the lens's own EV100.
    let pinned = render_at(&device, "pinned_sunny", sunny16.ev100(), &[], Lens::None);
    let physical = render(&device, "physical_f16", &[], Lens::Physical(sunny16));
    let worst = worst_difference(&pinned, &physical);
    eprintln!(
        "sunny 16 vs pinned EV {}: worst byte difference {worst}",
        sunny16.ev100()
    );
    assert_eq!(worst, 0, "sunny 16 renders as the camera pinned at its EV100");

    let wide = render(
        &device,
        "physical_f8",
        &[],
        Lens::Physical(PhysicalCamera {
            aperture_f_stops: 8.0,
            ..sunny16
        }),
    );
    let (l_pinned, _) = stats(&pinned);
    let (l_wide, _) = stats(&wide);
    eprintln!("luma pinned {l_pinned:.1} f/8 {l_wide:.1}");
    assert!(
        l_wide > l_pinned + 10.0,
        "two stops wider is brighter: {l_pinned} -> {l_wide}"
    );
}

/// A sunlit 16 m floor with a closed room (x -2..2, z -6..-2, 2.6 m high) whose front wall has a
/// doorway, two props and a warm lamp inside, and the camera at `eye` looking down -Z.
fn room_scene(eye: [f32; 3]) -> LabScene {
    let (floor, floor_idx) = regia_render::box_mesh([8.0, 0.05, 8.0]);
    let wall = |half: [f32; 3], at: [f32; 3]| {
        let (v, i) = regia_render::box_mesh(half);
        LabMesh {
            vertices: regia_render::translate_mesh(&v, at),
            indices: i,
            material: 1,
            ..LabMesh::unskinned()
        }
    };
    let (sphere, sphere_idx) = regia_render::uv_sphere(0.45, 24, 16);
    let (cube, cube_idx) = regia_render::box_mesh([0.4, 0.4, 0.4]);
    let mat = |c: [f32; 3]| LabMaterial {
        base_color: [c[0], c[1], c[2], 1.0],
        roughness: 0.7,
        ..LabMaterial::default()
    };
    let mut meshes = vec![
        LabMesh {
            vertices: regia_render::translate_mesh(&floor, [0.0, -0.05, -2.0]),
            indices: floor_idx,
            material: 0,
            ..LabMesh::unskinned()
        },
        wall([2.05, 1.3, 0.05], [0.0, 1.3, -6.0]),
        wall([0.05, 1.3, 2.05], [-2.0, 1.3, -4.0]),
        wall([0.05, 1.3, 2.05], [2.0, 1.3, -4.0]),
        wall([2.1, 0.05, 2.1], [0.0, 2.65, -4.0]),
        wall([0.7, 1.3, 0.05], [-1.3, 1.3, -2.0]),
        wall([0.7, 1.3, 0.05], [1.3, 1.3, -2.0]),
        wall([0.6, 0.3, 0.05], [0.0, 2.3, -2.0]),
    ];
    meshes.push(LabMesh {
        vertices: regia_render::translate_mesh(&sphere, [0.6, 0.45, -4.4]),
        indices: sphere_idx,
        material: 2,
        ..LabMesh::unskinned()
    });
    meshes.push(LabMesh {
        vertices: regia_render::translate_mesh(&cube, [-0.8, 0.4, -4.9]),
        indices: cube_idx,
        material: 3,
        ..LabMesh::unskinned()
    });
    LabScene {
        width: 320,
        height: 180,
        eye,
        target: [eye[0], eye[1] - 0.35, eye[2] - 1.0],
        fov_y: 60.0_f32.to_radians(),
        near: 0.05,
        far: 60.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: SUNNY_EV,
        ambient_lux: 8_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.45, -0.8, -0.35], [1.0, 0.97, 0.92], 100_000.0),
        local_lights: vec![LabLocalLight::Point(LabPointLight {
            position: [0.0, 2.3, -4.2],
            color: [1.0, 0.78, 0.55],
            intensity: 200_000.0,
            radius: 8.0,
            cast_shadows: false,
            source_radius: None,
        })],
        materials: vec![
            mat([0.55, 0.55, 0.52]),
            mat([0.78, 0.74, 0.66]),
            mat([0.75, 0.12, 0.08]),
            mat([0.1, 0.25, 0.7]),
        ],
        meshes,
    }
}

/// The room's look: a stop and a half brighter, warmer, a soft vignette, an eye range, fading in
/// over 3 m in front of the doorway.
fn room_volume() -> PostVolume {
    PostVolume {
        shape: PostVolumeShape::Box {
            half_extents: [2.0, 1.3, 2.0],
        },
        blend_distance: 3.0,
        priority: 1,
        overrides: PostOverrides {
            exposure_compensation: Some(2.5),
            white_balance_kelvin: Some(8000.0),
            saturation: Some(1.2),
            vignette: Some(0.35),
            eye: Some(regia_render::EyeRange {
                ev_min: 9.0,
                ev_max: 11.0,
                ..regia_render::EyeRange::default()
            }),
            ..PostOverrides::default()
        },
        ..PostVolume::default()
    }
}

#[test]
fn a_walk_from_sunlight_into_a_room_blends_its_look() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)");
    let volume = [(room_volume(), [0.0, 1.3, -4.0])];
    // Outside (beyond the band), at the band's start, half way, in the doorway, inside.
    let path = [6.0f32, 1.0, -0.5, -2.2, -2.8];
    let mut compensation = Vec::new();
    let mut inside = Vec::new();
    for (step, z) in path.iter().enumerate() {
        let lab = room_scene([0.0, 1.6, *z]);
        let (pixels, post) = render_scene(&device, &format!("walk_{step}_z{z}"), &lab, &volume, Lens::None);
        compensation.push(post.exposure_compensation);
        inside = pixels;
    }
    eprintln!("exposure compensation along the walk: {compensation:?}");
    assert_eq!(compensation[0], 0.0, "outside the band");
    assert_eq!(compensation[1], 0.0, "the band's outer edge");
    assert!(
        (compensation[2] - 1.25).abs() < 1.0e-4,
        "half way: {}",
        compensation[2]
    );
    assert_eq!(compensation[3], 2.5);
    assert_eq!(compensation[4], 2.5);
    assert!(
        compensation.windows(2).all(|w| w[1] >= w[0]),
        "monotonic: {compensation:?}"
    );

    let lab = room_scene([0.0, 1.6, -2.8]);
    let (plain, _) = render_scene(&device, "walk_inside_without_volume", &lab, &[], Lens::None);
    let (l_plain, _) = stats(&plain);
    let (l_inside, _) = stats(&inside);
    eprintln!("room luma without the volume {l_plain:.1}, with it {l_inside:.1}");
    assert!(
        l_inside > l_plain + 10.0,
        "the volume opens the room up: {l_plain} -> {l_inside}"
    );
    assert!(l_inside < 250.0, "and does not blow it out: {l_inside}");
}
