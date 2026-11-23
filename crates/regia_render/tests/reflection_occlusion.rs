// SPDX-License-Identifier: MPL-2.0
//! Reflection occlusion (`FrameQuality::reflection_occlusion`, `ssao.wgsl`).
//!
//! A closed room with one window, seen from outside under the IBL sky. Its polished metal floor
//! only sees the room (ceiling, walls): with the AO-based specular occlusion alone it mirrors
//! the open sky (the contact-scale AO kernel never reaches the far walls). With reflection
//! occlusion on, the floor seen through the window loses the sky reflection, while the
//! building's glossy facade, which faces the open sky, keeps its reflection.

use regia_core::glam::{Vec3, Vec4};
use regia_render::{
    ALPHA_PREMULTIPLIED, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh,
    LabScene, LabVertex, PostSettings, RenderDevice, SsaoMode, box_mesh, translate_mesh,
};

const W: u32 = 320;
const H: u32 = 240;
const EYE: [f32; 3] = [0.0, 2.4, 2.6];
const TARGET: [f32; 3] = [0.0, 0.2, -3.0];
const FOV_Y: f32 = 0.9;

/// Box slabs (centre, half extents, material): a 6 × 3 × 6 m room behind a 0.2 m front wall
/// (outer face at z = 0.2) with a 2.4 × 1.4 m window, on an outside ground.
fn slabs() -> Vec<([f32; 3], [f32; 3], usize)> {
    vec![
        // Interior floor (polished metal), ceiling, back and side walls.
        ([0.0, -0.1, -3.0], [3.2, 0.1, 3.2], 1),
        ([0.0, 3.1, -3.0], [3.2, 0.1, 3.2], 0),
        ([0.0, 1.5, -6.1], [3.2, 1.6, 0.1], 0),
        ([-3.1, 1.5, -3.0], [0.1, 1.6, 3.2], 0),
        ([3.1, 1.5, -3.0], [0.1, 1.6, 3.2], 0),
        // Front wall around the window (x in ±1.2, y in 0.8..2.2): sides, sill, lintel.
        ([-2.2, 1.5, 0.1], [1.0, 1.6, 0.1], 0),
        ([2.2, 1.5, 0.1], [1.0, 1.6, 0.1], 0),
        ([0.0, 0.35, 0.1], [1.2, 0.45, 0.1], 0),
        ([0.0, 2.7, 0.1], [1.2, 0.5, 0.1], 0),
        // Outside ground.
        ([0.0, -0.1, 5.0], [8.0, 0.1, 4.8], 2),
    ]
}

fn scene(quality: FrameQuality) -> (LabScene, FrameQuality) {
    let meshes = slabs()
        .into_iter()
        .map(|(c, h, material)| {
            let (v, indices) = box_mesh(h);
            LabMesh {
                vertices: translate_mesh(&v, c),
                indices,
                material,
                ..LabMesh::unskinned()
            }
        })
        .collect();
    let post = PostSettings {
        exposure_ev: 13.5,
        ..PostSettings::golden_deterministic()
    };
    let lab = LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: TARGET,
        fov_y: FOV_Y,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 13.5,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post),
        // High sun from behind the building: the room and the facade are in shadow, so the
        // indirect light (sky diffuse and reflection) is what the frame shows there.
        directional: DirectionalLightUniform::new([0.2, -0.8, 0.55], [1.0; 3], 60_000.0),
        local_lights: Vec::new(),
        materials: vec![
            // Walls and the facade: a glossy painted dielectric.
            LabMaterial {
                base_color: [0.55, 0.55, 0.55, 1.0],
                roughness: 0.3,
                ..LabMaterial::default()
            },
            // Polished metal floor.
            LabMaterial {
                base_color: [0.8, 0.8, 0.8, 1.0],
                metallic: 1.0,
                roughness: 0.25,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.4, 0.38, 0.35, 1.0],
                roughness: 0.9,
                ..LabMaterial::default()
            },
        ],
        meshes,
    };
    (lab, quality)
}

fn quality(reflection: bool) -> FrameQuality {
    FrameQuality {
        ssao: SsaoMode::GtaoHalfRes,
        reflection_occlusion: reflection,
        ..FrameQuality::LAB
    }
}

fn render(device: &RenderDevice, reflection: bool) -> Vec<u8> {
    let (lab, quality) = scene(quality(reflection));
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut renderer = FrameRenderer::new();
    renderer.render_offscreen(device, &frame).expect("frame").pixels
}

/// Screen pixel of a world point seen by a camera (right-handed, y up, y-down pixels).
fn project(eye: [f32; 3], target: [f32; 3], fov_y: f32, p: [f32; 3]) -> (f32, f32) {
    let view = regia_core::glam::camera::rh::view::look_at_mat4(
        Vec3::from_array(eye),
        Vec3::from_array(target),
        Vec3::Y,
    );
    let proj =
        regia_core::glam::camera::rh::proj::directx::perspective(fov_y, W as f32 / H as f32, 0.1, 200.0);
    let c = proj * view * Vec4::new(p[0], p[1], p[2], 1.0);
    let ndc = (c.x / c.w, c.y / c.w);
    ((ndc.0 * 0.5 + 0.5) * W as f32, (0.5 - ndc.1 * 0.5) * H as f32)
}

/// Screen pixel of a world point in the room view.
fn pixel(p: [f32; 3]) -> (f32, f32) {
    project(EYE, TARGET, FOV_Y, p)
}

/// Mean luma of the pixels inside the screen box of two world corners, inset by `inset` pixels,
/// and their count.
fn region(px: &[u8], a: [f32; 3], b: [f32; 3], inset: f32) -> (f64, usize) {
    let (pa, pb) = (pixel(a), pixel(b));
    let (x0, x1) = (pa.0.min(pb.0) + inset, pa.0.max(pb.0) - inset);
    let (y0, y1) = (pa.1.min(pb.1) + inset, pa.1.max(pb.1) - inset);
    let (mut luma, mut n) = (0.0, 0usize);
    for y in (y0.max(0.0) as u32)..(y1.min(H as f32) as u32) {
        for x in (x0.max(0.0) as u32)..(x1.min(W as f32) as u32) {
            let i = ((y * W + x) * 4) as usize;
            let (r, g, bl) = (f64::from(px[i]), f64::from(px[i + 1]), f64::from(px[i + 2]));
            luma += 0.2126 * r + 0.7152 * g + 0.0722 * bl;
            n += 1;
        }
    }
    (luma / n.max(1) as f64, n)
}

/// `REGIA_TEST_DUMP=<dir>` writes the frames there (to look at them).
fn dump(name: &str, px: &[u8]) {
    let Ok(dir) = regia_core::brand::env_var("REGIA_TEST_DUMP") else {
        return;
    };
    let path = std::path::Path::new(&dir).join(format!("{name}.png"));
    let written = std::fs::File::create(&path)
        .map_err(|e| e.to_string())
        .and_then(|file| {
            let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), W, H);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
            writer.write_image_data(px).map_err(|e| e.to_string())
        });
    eprintln!("dump {}: {written:?}", path.display());
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn a_room_behind_a_window_stops_reflecting_the_sky_and_the_facade_keeps_it() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("headless device");
    let off = render(&device, false);
    let on = render(&device, true);
    assert_eq!(
        on,
        render(&device, true),
        "reflection occlusion is not deterministic"
    );
    dump("room_off", &off);
    dump("room_on", &on);

    // The floor seen through the window: its world strip just inside the window's lower edge,
    // as far back as the window shows it.
    let floor = |px: &[u8]| region(px, [-1.0, 0.0, -1.4], [1.0, 0.0, -5.0], 3.0);
    let (floor_off, n) = floor(&off);
    let (floor_on, _) = floor(&on);
    eprintln!("interior floor ({n} px): luma {floor_off:.1} -> {floor_on:.1}");
    assert!(n > 200, "the floor is not in view through the window ({n} px)");
    assert!(
        floor_on < floor_off * 0.75,
        "the enclosed floor must lose its sky reflection: luma {floor_off:.1} -> {floor_on:.1}"
    );

    // The facade above the window faces the open sky (its mirror rays leave toward the camera's
    // side, dipping so little that they reach the ground only past the 8 m the rays test): its
    // reflection stays. (Lower down they reach the ground in front, which rightly takes the
    // sky out there.)
    let (a, b) = ([-2.9, 2.25, 0.2], [2.9, 3.0, 0.2]);
    let (l_off, n) = region(&off, a, b, 3.0);
    let (l_on, _) = region(&on, a, b, 3.0);
    eprintln!("upper facade ({n} px): luma {l_off:.1} -> {l_on:.1}");
    assert!(n > 200, "the upper facade is not in view ({n} px)");
    assert!(
        (l_on - l_off).abs() <= 0.03 * l_off.max(10.0),
        "the upper facade must keep its sky reflection: luma {l_off:.1} -> {l_on:.1}"
    );
}

const GLASS_EYE: [f32; 3] = [0.0, 2.0, 3.6];
const GLASS_TARGET: [f32; 3] = [0.0, 0.5, -1.0];
const GLASS_FOV_Y: f32 = 0.8;

/// A glass box (double-sided transmissive sheets, roof included) on an opaque floor, seen from outside
/// through its near pane, with screen-space refraction on (it binds the opaque depth to the
/// panes' draws). The far pane's inner face mirrors the floor inside the box, not the sky.
fn glass_box(reflection: bool) -> Vec<u8> {
    let device = RenderDevice::headless().expect("headless device");
    // Single double-sided sheets facing out of the box (centre, half-extent axes u and v, with
    // u × v the outward normal), as glTF canopies are modelled: the inner sides are back faces.
    let panes: [([f32; 3], [f32; 3], [f32; 3]); 5] = [
        ([0.0, 0.75, 1.0], [1.0, 0.0, 0.0], [0.0, 0.75, 0.0]),
        ([0.0, 0.75, -1.0], [-1.0, 0.0, 0.0], [0.0, 0.75, 0.0]),
        ([1.0, 0.75, 0.0], [0.0, 0.0, -1.0], [0.0, 0.75, 0.0]),
        ([-1.0, 0.75, 0.0], [0.0, 0.0, 1.0], [0.0, 0.75, 0.0]),
        ([0.0, 1.5, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]),
    ];
    let mut meshes: Vec<LabMesh> = vec![{
        let (v, indices) = box_mesh([4.0, 0.1, 4.0]);
        LabMesh {
            vertices: translate_mesh(&v, [0.0, -0.1, 0.0]),
            indices,
            material: 0,
            ..LabMesh::unskinned()
        }
    }];
    for (c, u, v) in panes {
        let (c, u, v) = (Vec3::from_array(c), Vec3::from_array(u), Vec3::from_array(v));
        let n = u.cross(v).normalize();
        let corners = [
            (c - u - v, [0.0, 1.0]),
            (c + u - v, [1.0, 1.0]),
            (c + u + v, [1.0, 0.0]),
            (c - u + v, [0.0, 0.0]),
        ];
        meshes.push(LabMesh {
            vertices: corners
                .iter()
                .map(|&(p, uv)| LabVertex::from_pos_normal_uv(p.to_array(), n.to_array(), uv))
                .collect(),
            indices: vec![0, 1, 2, 0, 2, 3],
            material: 1,
            ..LabMesh::unskinned()
        });
    }
    let post = PostSettings {
        exposure_ev: 13.5,
        ..PostSettings::golden_deterministic()
    };
    let lab = LabScene {
        width: W,
        height: H,
        eye: GLASS_EYE,
        target: GLASS_TARGET,
        fov_y: GLASS_FOV_Y,
        near: 0.05,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 13.5,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post),
        directional: DirectionalLightUniform::new([0.2, -0.8, 0.55], [1.0; 3], 60_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.25, 0.22, 0.2, 1.0],
                roughness: 0.8,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [1.0, 1.0, 1.0, 1.0],
                roughness: 0.05,
                transmission: 0.95,
                transmission_thickness: 0.01,
                alpha_mode: ALPHA_PREMULTIPLIED,
                double_sided: true,
                ..LabMaterial::default()
            },
        ],
        meshes,
    };
    let quality = FrameQuality {
        screen_space_refraction: true,
        ..quality(reflection)
    };
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut renderer = FrameRenderer::new();
    renderer.render_offscreen(&device, &frame).expect("frame").pixels
}

#[test]
fn the_far_pane_of_a_glass_box_reflects_the_floor_not_the_sky() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let off = glass_box(false);
    let on = glass_box(true);
    dump("glass_off", &off);
    dump("glass_on", &on);
    let lum = |px: &[u8], x: u32, y: u32| {
        let i = ((y * W + x) * 4) as usize;
        0.2126 * f64::from(px[i]) + 0.7152 * f64::from(px[i + 1]) + 0.0722 * f64::from(px[i + 2])
    };
    let (mut darker, mut brighter) = (0usize, 0usize);
    for y in 0..H {
        for x in 0..W {
            let (a, b) = (lum(&off, x, y), lum(&on, x, y));
            if b < a - 2.0 {
                darker += 1;
            } else if b > a + 2.0 {
                brighter += 1;
            }
        }
    }
    // The roof's top faces the open sky: its reflection stays.
    let (rx, ry) = project(GLASS_EYE, GLASS_TARGET, GLASS_FOV_Y, [0.0, 1.5, 0.0]);
    let (roof_off, roof_on) = (lum(&off, rx as u32, ry as u32), lum(&on, rx as u32, ry as u32));
    eprintln!("glass box: {darker} px darker, {brighter} px brighter; roof {roof_off:.1} -> {roof_on:.1}");
    // The side panes' inner faces and the far pane's lower band (its mirror rays reach the
    // floor inside) lose the sky; nothing gains light.
    assert!(
        darker > 2_000,
        "the panes' inner reflections must lose the sky ({darker} px darker)"
    );
    assert_eq!(brighter, 0, "reflection occlusion must not brighten the glass");
    assert!(
        (roof_on - roof_off).abs() <= 1.0,
        "the roof must keep its sky reflection: {roof_off:.1} -> {roof_on:.1}"
    );
}
