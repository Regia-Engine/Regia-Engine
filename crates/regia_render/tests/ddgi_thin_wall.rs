// SPDX-License-Identifier: MPL-2.0
//! A 0.1 m wall between a lit room and a dark room, probes on a 0.4 m grid (ADR-3947).
//!
//! Spec 27 § 2.2: the dark floor's luminance stays at or under 0.5 % of the lit floor.
//! The default tracer uses normal_bias 0.1 and min_frontface 0.12. The volume covers the room
//! surfaces and static screen reuse is disabled, so this measures fixed-frame convergence.
//! A probes-off, zero-ambient control checks that direct sunlight does not reach the dark floor.

use regia_core::glam::Mat4;
use regia_render::{
    DdgiSettings, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, box_mesh,
};

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip ddgi_thin_wall: {err}");
            None
        }
    }
}

const W: u32 = 160;
const H: u32 = 96;
/// Full thickness of the wall the two rooms share.
const SHARED_M: f32 = 0.1;
/// Full thickness of every other wall, floor slab and ceiling.
const THICK_M: f32 = 0.2;

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
    // Keep every probe update visible; static screen reuse would score an early frame.
    q.paged_sun_shadows = false;
    assert!(!q.reuse_static_screen());
    q.render_scale_permille = 1000;
    q
}

/// One 0.4 m cascade large enough that an eye in either room still places probes on both
/// sides of the shared wall, including the ceiling and floor bounce hits. Every probe
/// re-traces every frame; bricks bake on the caller.
fn probe_settings() -> DdgiSettings {
    DdgiSettings {
        cascades: 1,
        dims: [24, 16, 16],
        spacing: 0.4,
        update_divisor: 1,
        hysteresis: 0.9,
        min_proxy_radius: 0.2,
        bake_on_caller: true,
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

/// Mean BT.709 luma of a pixel rectangle, decoded to linear light. Same rectangle the sealed
/// room uses, so the fraction is read the same way.
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

fn scene(eye: [f32; 3], target: [f32; 3], meshes: Vec<LabMesh>) -> LabScene {
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
        materials: vec![grey(0.6)],
        meshes,
    }
}

fn converge(renderer: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene, frames: u32) -> Vec<u8> {
    let mut last = Vec::new();
    for _ in 0..frames {
        let frame = lab.as_frame_scene_with_quality(quality());
        last = renderer
            .render_offscreen(device, &frame)
            .unwrap_or_else(|err| panic!("frame: {err}"))
            .pixels;
    }
    last
}

/// Dark room on −x, lit room on +x, one shared wall of [`SHARED_M`] centred at x = 0.
/// The lit room has no ceiling and no +x wall, so the sun (from +x) lights its floor.
/// The dark room is closed. Other surfaces are [`THICK_M`].
fn two_rooms(wall_x: f32) -> Vec<LabMesh> {
    let shared = SHARED_M * 0.5;
    let thick = THICK_M * 0.5;
    let y_mid = 1.2;
    // Dark air is x = −2.4 .. −shared, lit air is x = shared .. 2.4, z = ±1.2, y = 0 .. 2.4.
    // Floors meet at x = 0 under the shared wall and do not cross into the other room's air.
    let dark_x = -1.2 - thick * 0.5;
    let lit_x = 1.2 + thick * 0.5;
    let span_x = 1.2 + thick * 0.5;
    let span_z = 1.2 + thick;
    let mut meshes = vec![
        // Ground under both rooms, clear of the floors.
        block([0.0, -0.8, 0.0], [8.0, 0.25, 4.0]),
        // Floors, top at y = 0, full thickness 0.5 m.
        block([dark_x, -0.25, 0.0], [span_x, 0.25, span_z]),
        block([lit_x, -0.25, 0.0], [span_x, 0.25, span_z]),
        // Dark ceiling, 0.2 m, sitting on y = 2.4.
        block([dark_x, 2.4 + thick, 0.0], [span_x, thick, span_z]),
        // Shared wall, 0.1 m, from the floor to the ceiling and across the front and back walls.
        block([0.0, y_mid, 0.0], [shared, y_mid + thick, span_z]),
        // Dark outer wall.
        block([-2.4 - thick, y_mid, 0.0], [thick, y_mid + thick, span_z]),
        // Front and back walls. The lit room has no ceiling and no +x wall, so the sun reaches it.
        block([dark_x, y_mid, 1.2 + thick], [span_x, y_mid + thick, thick]),
        block([dark_x, y_mid, -1.2 - thick], [span_x, y_mid + thick, thick]),
        block([lit_x, y_mid, 1.2 + thick], [span_x, y_mid + thick, thick]),
        block([lit_x, y_mid, -1.2 - thick], [span_x, y_mid + thick, thick]),
    ];
    for mesh in &mut meshes {
        mesh.model[3][0] += wall_x;
    }
    meshes
}

/// Serialises device creation in this binary (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Dark-floor luma and lit-floor luma. The lit eye is the reference (the light that should
/// not appear in the dark room). `None` without a device.
fn floor_pair(device: &RenderDevice, probes: bool, wall_x: f32) -> (f32, f32) {
    let meshes = two_rooms(wall_x);
    // Eyes sit 1.2 m off the wall, looking along +z at the floor, the same kind of view the
    // sealed room scores. At wall_x = 0 the wall occupies a probe plane; at 0.2 it lies
    // halfway between planes. The physical scene and cameras translate together.
    let mut lit = scene(
        [1.2 + wall_x, 1.5, -0.6],
        [1.2 + wall_x, 0.2, 0.8],
        meshes.clone(),
    );
    let mut dark = scene([-1.2 + wall_x, 1.5, -0.6], [-1.2 + wall_x, 0.2, 0.8], meshes);
    if !probes {
        lit.ambient_lux = 0.0;
        dark.ambient_lux = 0.0;
    }
    let settings = probe_settings();
    assert_eq!(settings.normal_bias, 0.1);
    assert_eq!(settings.min_frontface, 0.12);
    let mut lit_renderer = FrameRenderer::new();
    lit_renderer.set_ddgi_settings(settings.clone());
    lit_renderer.set_ddgi_enabled(Some(probes));
    let lit_px = converge(&mut lit_renderer, device, &lit, 36);
    assert!(
        lit_renderer.ddgi_active() == probes,
        "the probe override is honored"
    );
    let lit_luma = linear_luma(&lit_px, 40, 50, 120, 90);
    drop(lit_renderer);
    let mut dark_renderer = FrameRenderer::new();
    dark_renderer.set_ddgi_settings(settings);
    dark_renderer.set_ddgi_enabled(Some(probes));
    let dark_px = converge(&mut dark_renderer, device, &dark, 64);
    let dark_luma = linear_luma(&dark_px, 40, 50, 120, 90);
    (dark_luma, lit_luma)
}

#[test]
fn a_tenth_meter_shared_wall_stays_under_the_leak_bound() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    // Cover both a wall containing a probe plane and one between the adjacent planes.
    for wall_x in [0.0, 0.2] {
        let (direct_dark, direct_lit) = floor_pair(&device, false, wall_x);
        eprintln!(
            "thin-wall wall_x={wall_x} probes=off ambient=0 dark={direct_dark:.6} lit={direct_lit:.6} leak={:.6}",
            direct_dark / direct_lit.max(1e-9)
        );
        assert!(direct_lit > 0.1, "the direct-light reference is lit");
        assert!(
            direct_dark <= 0.005 * direct_lit,
            "direct light leaks into the dark room: {direct_dark} / {direct_lit}"
        );
        let (dark, lit) = floor_pair(&device, true, wall_x);
        let ratio = dark / lit.max(1e-9);
        eprintln!(
            "thin-wall wall_x={wall_x} normal_bias=0.1 min_frontface=0.12 dark={dark:.6} lit={lit:.6} leak={ratio:.6}"
        );
        assert!(lit > 0.1, "the lit floor must be lit ({lit})");
        // Spec 27 § 2.2, the sealed-room bound: 0.5 % of the light that should be absent.
        assert!(
            dark <= 0.005 * lit,
            "light leaks through the 0.1 m wall at the default bias: dark {dark:.6} vs lit {lit:.6} ({:.3} %)",
            ratio * 100.0
        );
    }
}
