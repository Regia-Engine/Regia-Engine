// SPDX-License-Identifier: MPL-2.0
//! The standard GI validation scenes (ADR-423): small lab scenes whose indirect light the
//! real-time GI techniques are scored on against the path tracer (`specs/27` § 2.2 GI rows,
//! § 3.2 acceptance list).
//!
//! - [`trench`]: a sunlit wall bouncing onto a floor in its neighbour's shadow (outdoor bounce,
//!   the baked-GI runtime test's layout);
//! - [`color_room`]: a room open at the front, red and green side walls, the sun through the
//!   opening on the red wall and the floor, the ceiling and the back corners lit only
//!   indirectly (colour bleeding, multi-bounce);
//! - [`sealed_box`]: the § 2.2 leak case, a closed box under the sun and the sky with the camera
//!   inside: the reference is black, every bit of light inside is a leak.
//!
//! Every scene is opaque, dielectric and rough, lit by the sun and a uniform sky (`ibl` off,
//! `ambient_lux` the sky's illuminance), so the tracer and the frame see the same lights. Each
//! comes with the probe placement its techniques need: a box for the baked volume and the
//! dynamic probes' settings.

use regia_render::{
    DdgiSettings, DirectionalLightUniform, LabMaterial, LabMesh, LabScene, PostSettings, box_mesh,
};

/// Width of every GI validation frame.
pub const GI_WIDTH: u32 = 160;
/// Height of every GI validation frame.
pub const GI_HEIGHT: u32 = 96;

/// A GI validation scene and the probe placement its techniques use.
#[derive(Clone, Debug)]
pub struct GiScene {
    /// Name (`[a-z0-9_]`): the record file and the table rows use it.
    pub name: &'static str,
    /// The frame both renderers draw.
    pub lab: LabScene,
    /// Corners of the baked detail brick (world space, metres).
    pub bake_bounds: ([f32; 3], [f32; 3]),
    /// Baked probe spacing, metres.
    pub bake_spacing: f32,
    /// The dynamic probes' settings for this scene.
    pub ddgi: DdgiSettings,
    /// Luminance the relative errors divide by when the reference indirect light is about zero
    /// (the lit exterior ground of [`sealed_box`]); `None` for the mean reference luminance.
    pub scale: Option<f32>,
}

fn translation(offset: [f32; 3]) -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [offset[0], offset[1], offset[2], 1.0],
    ]
}

fn block(centre: [f32; 3], half: [f32; 3], material: usize) -> LabMesh {
    let (vertices, indices) = box_mesh(half);
    LabMesh {
        vertices,
        indices,
        material,
        model: translation(centre),
        ..LabMesh::unskinned()
    }
}

/// A rough dielectric (roughness 0.95): its specular lobe stays small, so the frame's indirect
/// light is mostly the diffuse GI under test.
fn lambert(rgb: [f32; 3]) -> LabMaterial {
    LabMaterial {
        base_color: [rgb[0], rgb[1], rgb[2], 1.0],
        metallic: 0.0,
        roughness: 0.95,
        ..LabMaterial::default()
    }
}

fn lab(
    eye: [f32; 3],
    target: [f32; 3],
    exposure_ev: f32,
    sky_lux: f32,
    sun: DirectionalLightUniform,
    materials: Vec<LabMaterial>,
    meshes: Vec<LabMesh>,
) -> LabScene {
    let mut post = PostSettings::golden_deterministic();
    post.exposure_ev = exposure_ev;
    LabScene {
        width: GI_WIDTH,
        height: GI_HEIGHT,
        eye,
        target,
        fov_y: 1.0,
        near: 0.05,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev,
        ambient_lux: sky_lux,
        double_sided: false,
        post: Some(post),
        directional: sun,
        local_lights: Vec::new(),
        materials,
        meshes,
    }
}

/// Dynamic probe settings for a scene of probe spacing `spacing`: two cascades of `dims`, every
/// probe re-traced every frame and a short history (hysteresis 0.9), so a fixed number of frames
/// converges; small meshes traced; bricks baked on the calling thread.
///
/// `dims` must hold what the plate's camera sees inside a cascade's full-weight window: `eye / s −
/// lift − d / 2 + 1.75` to `eye / s − lift + d / 2 − 3.75` cells of spacing `s` (`lift` = 0.1 d, in y
/// only, for a layout this fine). A pixel outside it mixes in the flat sky term instead of probe
/// light, which reads bright under the sky and black under the sun (ADR-2605; `regia_pt`
/// `the_plate_layouts_hold_what_the_camera_sees`).
fn ddgi(spacing: f32, dims: [u32; 3]) -> DdgiSettings {
    DdgiSettings {
        cascades: 2,
        dims,
        spacing,
        spacing_ratio: 3.0,
        update_divisor: 1,
        hysteresis: 0.9,
        min_proxy_radius: 0.2,
        bake_on_caller: true,
        ..DdgiSettings::default()
    }
}

/// A trench 6 m wide with 4 m walls, long in x, under a 100 klx sun travelling −z and down at
/// 35° and an 8 klx sky: the far wall's inner face (orange) is sunlit, the floor is in the near
/// wall's shadow, and a grey block stands on it.
#[must_use]
pub fn trench() -> GiScene {
    let meshes = vec![
        block([0.0, -0.25, 0.0], [16.0, 0.25, 8.0], 0),
        block([0.0, 2.0, -3.15], [15.0, 2.0, 0.15], 1),
        block([0.0, 2.0, 3.15], [15.0, 2.0, 0.15], 2),
        block([1.5, 0.5, -1.0], [0.5, 0.5, 0.5], 2),
    ];
    GiScene {
        name: "trench",
        lab: lab(
            [-3.0, 1.6, 2.2],
            [2.0, 0.6, -1.5],
            13.5,
            8_000.0,
            DirectionalLightUniform::new([0.0, -0.574, -0.819], [1.0, 0.97, 0.92], 100_000.0),
            vec![lambert([0.45; 3]), lambert([0.85, 0.5, 0.3]), lambert([0.6; 3])],
            meshes,
        ),
        bake_bounds: ([-8.0, 0.3, -2.7], [8.0, 3.6, 2.7]),
        bake_spacing: 1.0,
        // The 1 m cascade holds the far wall's face and 2.65 m of its height; the 3 m one the rest, and
        // the trench's far end.
        ddgi: ddgi(1.0, [22, 12, 14]),
        scale: None,
    }
}

/// A 2.4 m room open at the front (+z): white floor, ceiling and back wall, a red left wall and
/// a green right wall. A 60 klx sun travels in through the opening (−z, toward −x, 33° high)
/// onto the red wall, the back wall and the front of the floor; the ceiling and the back
/// corners get indirect light only. A 6 klx sky. The floor runs out under the camera.
#[must_use]
pub fn color_room() -> GiScene {
    let t = 0.06;
    let meshes = vec![
        // Floor, running 2.3 m out of the opening.
        block([0.0, -t, 1.15], [1.2 + 2.0 * t, t, 2.35 + t], 0),
        // Ceiling.
        block([0.0, 2.4 + t, 0.0], [1.2 + 2.0 * t, t, 1.2 + t], 0),
        // Back wall, wide enough to close the back corners.
        block([0.0, 1.2, -1.2 - t], [1.2 + 2.0 * t, 1.2, t], 0),
        // Left (red) and right (green) walls.
        block([-1.2 - t, 1.2, 0.0], [t, 1.2, 1.2], 1),
        block([1.2 + t, 1.2, 0.0], [t, 1.2, 1.2], 2),
    ];
    GiScene {
        name: "color_room",
        lab: lab(
            [0.3, 1.3, 3.0],
            [0.0, 1.0, 0.0],
            12.5,
            6_000.0,
            DirectionalLightUniform::new([-0.35, -0.55, -0.76], [1.0, 0.97, 0.92], 60_000.0),
            vec![
                lambert([0.75, 0.75, 0.72]),
                lambert([0.8, 0.08, 0.06]),
                lambert([0.08, 0.55, 0.12]),
            ],
            meshes,
        ),
        bake_bounds: ([-1.0, 0.2, -1.0], [1.0, 2.2, 1.4]),
        bake_spacing: 0.4,
        // The 0.4 m cascade holds the room to the ceiling and the back wall: the 1.2 m one alone covers
        // the view too but reads the ceiling 1.34 times the tracer, spacing half the room.
        ddgi: ddgi(0.4, [12, 18, 24]),
        scale: None,
    }
}

/// Albedo of [`sealed_box`]'s exterior ground.
const SEALED_GROUND_ALBEDO: f32 = 0.4;
/// Sky illuminance of [`sealed_box`], lux.
const SEALED_SKY_LUX: f32 = 10_000.0;
/// Sun illuminance of [`sealed_box`] (normal incidence), lux.
const SEALED_SUN_LUX: f32 = 100_000.0;
/// Sun direction of travel of [`sealed_box`] (45° high).
const SEALED_SUN_DIR: [f32; 3] = [0.5, -std::f32::consts::FRAC_1_SQRT_2, -0.5];

/// The § 2.2 leak case: a closed grey box (2 m inside, 0.1 m walls) on a large ground under a
/// 100 klx sun 45° high and a 10 klx sky, the camera inside. No light reaches the inside, so
/// the reference is black; the scale is the lit exterior ground's luminance, and the relative
/// errors read as the leak fraction (§ 2.2 bound: 0.5 %).
#[must_use]
pub fn sealed_box() -> GiScene {
    let w = 0.1;
    let meshes = vec![
        // Exterior ground, its top at y = −0.2 below the box's floor slab.
        block([0.0, -0.45, 0.0], [30.0, 0.25, 30.0], 1),
        // Floor and roof slabs.
        block([0.0, -w, 0.0], [1.0 + 2.0 * w, w, 1.0 + 2.0 * w], 0),
        block([0.0, 2.0 + w, 0.0], [1.0 + 2.0 * w, w, 1.0 + 2.0 * w], 0),
        // Four walls, overlapping at the corners so no crack lets light in.
        block([-1.0 - w, 1.0, 0.0], [w, 1.0 + w, 1.0 + 2.0 * w], 0),
        block([1.0 + w, 1.0, 0.0], [w, 1.0 + w, 1.0 + 2.0 * w], 0),
        block([0.0, 1.0, -1.0 - w], [1.0 + 2.0 * w, 1.0 + w, w], 0),
        block([0.0, 1.0, 1.0 + w], [1.0 + 2.0 * w, 1.0 + w, w], 0),
    ];
    let sin_elevation = -SEALED_SUN_DIR[1]
        / (SEALED_SUN_DIR[0] * SEALED_SUN_DIR[0]
            + SEALED_SUN_DIR[1] * SEALED_SUN_DIR[1]
            + SEALED_SUN_DIR[2] * SEALED_SUN_DIR[2])
            .sqrt();
    // A Lambertian ground: L = ρ / π · (E_sun · sin(elevation) + E_sky).
    let ground =
        SEALED_GROUND_ALBEDO / std::f32::consts::PI * (SEALED_SUN_LUX * sin_elevation + SEALED_SKY_LUX);
    GiScene {
        name: "sealed_box",
        lab: lab(
            [0.5, 1.2, 0.7],
            [-0.6, 0.6, -0.8],
            13.0,
            SEALED_SKY_LUX,
            DirectionalLightUniform::new(SEALED_SUN_DIR, [1.0, 1.0, 1.0], SEALED_SUN_LUX),
            vec![lambert([0.7; 3]), lambert([SEALED_GROUND_ALBEDO; 3])],
            meshes,
        ),
        bake_bounds: ([-0.8, 0.2, -0.8], [0.8, 1.8, 0.8]),
        bake_spacing: 0.4,
        ddgi: ddgi(0.4, [10, 10, 10]),
        scale: Some(ground),
    }
}

/// Every standard GI validation scene, in table order.
#[must_use]
pub fn gi_validation_scenes() -> Vec<GiScene> {
    vec![trench(), color_room(), sealed_box()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenes_have_distinct_valid_names_and_one_frame_size() {
        let scenes = gi_validation_scenes();
        let mut names: Vec<&str> = scenes.iter().map(|s| s.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), scenes.len());
        for s in &scenes {
            assert!(
                s.name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            );
            assert_eq!((s.lab.width, s.lab.height), (GI_WIDTH, GI_HEIGHT));
            assert!(
                !s.lab.ibl && s.lab.ambient_lux > 0.0,
                "{}: sun and uniform sky only",
                s.name
            );
            assert!(s.lab.local_lights.is_empty());
            for m in &s.lab.materials {
                assert!(
                    m.metallic == 0.0 && m.roughness >= 0.9,
                    "{}: rough dielectrics",
                    s.name
                );
            }
            for m in &s.lab.meshes {
                assert!(m.material < s.lab.materials.len());
            }
            let (lo, hi) = s.bake_bounds;
            assert!((0..3).all(|a| lo[a] < hi[a]) && s.bake_spacing > 0.0);
            assert!(s.ddgi.bake_on_caller && s.ddgi.update_divisor == 1);
        }
    }

    #[test]
    fn the_sealed_box_scale_is_the_lit_ground_luminance() {
        let s = sealed_box();
        let scale = s.scale.expect("the leak scene names its scale");
        // 0.4 / π · (100 000 · sin 45° + 10 000) ≈ 10 277 cd/m².
        assert!((scale - 10_277.0).abs() < 5.0, "{scale}");
        assert!(trench().scale.is_none() && color_room().scale.is_none());
    }

    #[test]
    fn the_sealed_box_camera_is_inside_and_the_box_is_closed() {
        let s = sealed_box();
        let e = s.lab.eye;
        assert!(e[0].abs() < 1.0 && e[2].abs() < 1.0 && e[1] > 0.0 && e[1] < 2.0);
        let inside = |p: [f32; 3]| p[0].abs() < 1.0 && p[2].abs() < 1.0 && p[1] > 0.0 && p[1] < 2.0;
        assert!(inside(s.lab.eye) && inside(s.lab.target));
    }
}
