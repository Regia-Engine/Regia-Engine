// SPDX-License-Identifier: MPL-2.0
//! Glass front layer (`frame.rs` "glass front layer", `lab_forward.wgsl` `front_layer_weight`).
//!
//! Two parallel dark transmissive panes (transmittance ≈ 0.018, DO27's smoked canopy) under a
//! sun mirrored into the camera, with screen-space refraction on (Desktop High):
//! - the far pane's own reflection is seen through the near pane's transmittance, so adding it
//!   behind the near pane changes the pixel by a few percent of what it shows alone (before:
//!   its whole reflection, the far side of a canopy read through dark glass);
//! - one mesh holding both panes (its multiply and add each cover both) and two meshes (sorted
//!   back to front) give the same frame.

use regia_core::glam::{Mat4, Vec3};
use regia_render::{
    ALPHA_PREMULTIPLIED, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh,
    LabScene, LabVertex, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
};

const W: u32 = 160;
const H: u32 = 120;
const NEAR_Y: f32 = 0.0;
const FAR_Y: f32 = -0.3;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A thin 2 × 2 m slab at height `y`, its vertices in world space (identity model).
fn pane(y: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let (mut vertices, indices) = regia_render::box_mesh([1.0, 0.002, 1.0]);
    for v in &mut vertices {
        v.position[1] += y;
    }
    (vertices, indices)
}

fn mesh(parts: &[(Vec<LabVertex>, Vec<u32>)], material: usize) -> LabMesh {
    let mut out = LabMesh {
        material,
        ..LabMesh::unskinned()
    };
    for (vertices, indices) in parts {
        let base = out.vertices.len() as u32;
        out.vertices.extend_from_slice(vertices);
        out.indices.extend(indices.iter().map(|i| i + base));
    }
    out
}

#[derive(Clone, Copy)]
enum Panes {
    None,
    Near,
    Far,
    /// Both panes in one mesh.
    OneMesh,
    /// Each pane its own mesh.
    TwoMeshes,
}

/// Mean HDR luminance (cd/m²) over the central 24 × 24 pixels.
fn centre_luminance(panes: Panes) -> f32 {
    let floor = LabMaterial {
        base_color: [0.0, 0.0, 0.0, 1.0],
        roughness: 1.0,
        ..LabMaterial::default()
    };
    let glass = LabMaterial {
        base_color: [0.02, 0.02, 0.02, 1.0],
        roughness: 0.35,
        transmission: 0.92,
        ior: 1.5,
        // A volume thickness: the pane refracts, so the frame takes the refraction copy and
        // the front layer with it.
        transmission_thickness: 0.01,
        alpha_mode: ALPHA_PREMULTIPLIED,
        ..LabMaterial::default()
    };
    let (floor_v, floor_i) = regia_render::box_mesh([10.0, 0.01, 10.0]);
    let mut meshes = vec![LabMesh {
        vertices: floor_v,
        indices: floor_i,
        material: 0,
        model: Mat4::from_translation(Vec3::new(0.0, -2.0, 0.0)).to_cols_array_2d(),
        ..LabMesh::unskinned()
    }];
    match panes {
        Panes::None => {}
        Panes::Near => meshes.push(mesh(&[pane(NEAR_Y)], 1)),
        Panes::Far => meshes.push(mesh(&[pane(FAR_Y)], 1)),
        Panes::OneMesh => meshes.push(mesh(&[pane(NEAR_Y), pane(FAR_Y)], 1)),
        Panes::TwoMeshes => {
            meshes.push(mesh(&[pane(NEAR_Y)], 1));
            meshes.push(mesh(&[pane(FAR_Y)], 1));
        }
    }
    // The eye looks down at 45°; the sun sits at the eye's mirror direction.
    let eye = [0.0, 3.0, 3.0];
    let to_sun = Vec3::new(0.0, 1.0, -1.0).normalize();
    let post = PostSettings {
        exposure_ev: 12.0,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: W,
        height: H,
        eye,
        target: [0.0, 0.0, 0.0],
        fov_y: 0.6,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new((-to_sun).to_array(), [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![floor, glass],
        meshes,
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.screen_space_refraction = true;
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let device = device();
    let mut renderer = FrameRenderer::new();
    renderer.request_hdr_capture();
    renderer.render_offscreen(&device, &frame).expect("frame");
    let hdr = renderer
        .take_hdr_capture(&device)
        .expect("hdr readback")
        .expect("the frame ran the post stack");
    let (cx, cy) = (hdr.width / 2, hdr.height / 2);
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in cy - 12..cy + 12 {
        for x in cx - 12..cx + 12 {
            let [r, g, b] = hdr.rgb[(y * hdr.width + x) as usize];
            sum += 0.2126 * r + 0.7152 * g + 0.0722 * b;
            n += 1.0;
        }
    }
    sum / n
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn a_pane_behind_dark_glass_adds_its_reflection_through_the_near_pane() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let bg = centre_luminance(Panes::None);
    let near = centre_luminance(Panes::Near);
    let far = centre_luminance(Panes::Far);
    let both = centre_luminance(Panes::OneMesh);
    eprintln!("background {bg}, near {near}, far {far}, both (one mesh) {both}");
    // Each pane alone shows the sun's highlight over a dark background.
    assert!(
        near > 20.0 * bg.max(1e-3) && far > 20.0 * bg.max(1e-3),
        "{near} {far} {bg}"
    );
    // Behind the near pane (transmittance ≈ 0.018) the far one adds a few percent of its own
    // light, not all of it (the old within-draw order added it unattenuated: both ≈ near + far).
    let added = both - near;
    assert!(
        added >= -0.02 * near,
        "the far pane darkened the near one: {added}"
    );
    assert!(
        added < 0.1 * (far - bg),
        "far pane seen unattenuated: +{added} of {}",
        far - bg
    );
}

#[test]
fn one_mesh_and_two_meshes_composite_alike() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let one = centre_luminance(Panes::OneMesh);
    let two = centre_luminance(Panes::TwoMeshes);
    eprintln!("one mesh {one}, two meshes {two}");
    assert!((one - two).abs() <= 0.02 * one.max(two) + 1e-3, "{one} vs {two}");
}
