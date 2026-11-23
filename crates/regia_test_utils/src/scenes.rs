// SPDX-License-Identifier: MPL-2.0
//! Deterministic render-lab scenes for golden tests (`specs/13` § 13).

use glam::{Mat4, Quat, Vec2, Vec3};
use regia_render::{
    DirectionalLightUniform, LabLocalLight, LabMaterial, LabMesh, LabPointLight, LabScene, LabSpotLight,
    LabTexture, LabVertex, PostSettings, PrimitiveShape, RenderDevice, RenderError, TonemapOperator,
    box_mesh, generate_vertex_tangents, render_lab_scene, uv_sphere,
};

/// Fixed width for renderer goldens.
pub const GOLDEN_WIDTH: u32 = 512;
/// Fixed height for renderer goldens.
pub const GOLDEN_HEIGHT: u32 = 512;

/// EV100 for a scene lit by a ~100 klx sun (the "sunny 16" value, `specs/13` § 6).
pub const SUN_EV100: f32 = 15.0;

/// Shared sun for lab scenes: a physically-based ~100,000 lux directional light.
///
/// The direction is chosen so the light travels *toward* the camera side of the
/// standard lab scenes (front lighting); a back-lit setup leaves camera-facing
/// surfaces dark.
#[must_use]
pub fn lab_directional_light() -> DirectionalLightUniform {
    DirectionalLightUniform::new([-0.35, -0.75, -0.55], [1.0, 0.98, 0.95], 100_000.0)
}

/// Renders a metal × roughness grid (5×5) with distinct materials per sphere.
///
/// Every combination is a separate mesh with its own material on the clustered
/// lab frame, exposed at [`SUN_EV100`] like the other goldens.
pub fn material_sphere_grid_scene() -> LabScene {
    let grid = 5usize;
    let spacing = 1.15f32;
    let radius = 0.42f32;
    let offset = (grid as f32 - 1.0) * spacing * 0.5;

    let mut meshes: Vec<LabMesh> = Vec::new();
    let mut materials: Vec<LabMaterial> = Vec::new();

    for row in 0..grid {
        for col in 0..grid {
            let metallic = col as f32 / (grid - 1).max(1) as f32;
            let roughness = 0.05 + (row as f32 / (grid - 1).max(1) as f32) * 0.9;
            let cx = col as f32 * spacing - offset;
            let cz = row as f32 * spacing - offset;
            let mut sphere = lab_sphere([cx, 0.0, cz], radius);
            sphere.material = row * grid + col;
            meshes.push(sphere);
            materials.push(LabMaterial {
                base_color: [0.9, 0.9, 0.92, 1.0],
                metallic,
                roughness,
                ..LabMaterial::default()
            });
        }
    }

    LabScene {
        width: GOLDEN_WIDTH,
        height: GOLDEN_HEIGHT,
        eye: [0.0, 3.6, 5.6],
        target: [0.0, 0.1, 0.0],
        fov_y: 45.0_f32.to_radians(),
        near: 0.1,
        far: 40.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: SUN_EV100,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: None,
        directional: lab_directional_light(),
        local_lights: Vec::new(),
        materials,
        meshes,
    }
}

/// Renders [`material_sphere_grid_scene`].
pub fn render_material_sphere_grid(device: &RenderDevice) -> Result<Vec<u8>, RenderError> {
    Ok(render_lab_scene(device, &material_sphere_grid_scene())?.pixels)
}

fn lab_sphere(center: [f32; 3], radius: f32) -> LabMesh {
    let (verts, indices) = uv_sphere(radius, 24, 16);
    let vertices = verts
        .iter()
        .map(|v| {
            let mut lab = *v;
            lab.position[0] += center[0];
            lab.position[1] += center[1];
            lab.position[2] += center[2];
            lab
        })
        .collect();
    LabMesh {
        vertices,
        indices,
        material: 0,
        ..LabMesh::unskinned()
    }
}

/// Point light (red) and spot light (green) on two spheres over a floor, with a
/// depth prepass.
///
/// Physically-plausible units: a 1,500-lumen red bulb and a 110-candela green
/// spot in a dark room, exposed at EV100 3.0 — the light pools on the floor and
/// spheres read clearly while the room stays dark at the edges.
pub fn clustered_lights_scene() -> LabScene {
    let h = 5.0;
    let corners = [[-h, 0.0, -h], [h, 0.0, -h], [h, 0.0, h], [-h, 0.0, h]];
    let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    let floor_verts = corners
        .into_iter()
        .zip(uvs)
        .map(|(position, uv)| LabVertex {
            position,
            normal: [0.0, 1.0, 0.0],
            uv,
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect();
    LabScene {
        width: GOLDEN_WIDTH,
        height: GOLDEN_HEIGHT,
        eye: [0.0, 1.5, 4.4],
        target: [0.0, 0.45, 0.0],
        fov_y: 50.0_f32.to_radians(),
        near: 0.1,
        far: 40.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 3.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -1.0, 0.15], [1.0, 0.98, 0.95], 0.15),
        local_lights: vec![
            LabLocalLight::Point(LabPointLight {
                position: [-1.0, 1.8, 1.3],
                color: [1.0, 0.08, 0.05],
                intensity: 1500.0,
                radius: 7.0,
                cast_shadows: false,
                source_radius: None,
            }),
            LabLocalLight::Spot(LabSpotLight {
                position: [2.0, 2.4, 1.8],
                direction: [-0.5, -0.7, -0.4],
                color: [0.08, 1.0, 0.2],
                intensity: 110.0,
                range: 9.0,
                inner_angle: 0.25,
                outer_angle: 0.65,
                cast_shadows: false,
                source_radius: None,
            }),
        ],
        materials: vec![LabMaterial {
            base_color: [0.82, 0.82, 0.84, 1.0],
            roughness: 0.35,
            metallic: 0.0,
            ..LabMaterial::default()
        }],
        meshes: vec![
            LabMesh {
                vertices: floor_verts,
                indices: vec![0, 2, 1, 0, 3, 2],
                material: 0,
                ..LabMesh::unskinned()
            },
            lab_sphere([-0.9, 0.5, 0.0], 0.5),
            lab_sphere([0.9, 0.5, 0.0], 0.5),
        ],
    }
}

/// Renders [`clustered_lights_scene`].
pub fn render_clustered_lights(device: &RenderDevice) -> Result<Vec<u8>, RenderError> {
    Ok(render_lab_scene(device, &clustered_lights_scene())?.pixels)
}

/// Directional cascades plus a shadowed point and spot on a floor and sphere.
///
/// A physically-based ~100 klx sun through EV100 exposure (`specs/13` § 6): the
/// sun-lit floor reads mid-bright, not flat white.
pub fn light_shadows_scene() -> LabScene {
    let h = 8.0;
    let corners = [[-h, 0.0, -h], [h, 0.0, -h], [h, 0.0, h], [-h, 0.0, h]];
    let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    let floor_verts = corners
        .into_iter()
        .zip(uvs)
        .map(|(position, uv)| LabVertex {
            position,
            normal: [0.0, 1.0, 0.0],
            uv,
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect();
    let sphere = lab_sphere([0.0, 0.45, 0.0], 0.45);
    LabScene {
        width: GOLDEN_WIDTH,
        height: GOLDEN_HEIGHT,
        eye: [1.8, 2.4, 3.4],
        target: [0.0, 0.25, 0.0],
        fov_y: 45.0_f32.to_radians(),
        near: 0.1,
        far: 30.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: SUN_EV100,
        ambient_lux: 10_000.0,
        double_sided: false,
        post: None,
        directional: lab_directional_light(),
        local_lights: vec![
            LabLocalLight::Point(LabPointLight {
                position: [-1.3, 1.9, 0.2],
                color: [1.0, 0.35, 0.2],
                intensity: 350.0,
                radius: 5.5,
                cast_shadows: true,
                source_radius: None,
            }),
            LabLocalLight::Spot(LabSpotLight {
                position: [1.5, 2.1, 1.3],
                direction: [-0.45, -0.75, -0.4],
                color: [0.35, 0.55, 1.0],
                intensity: 60.0,
                range: 7.0,
                inner_angle: 0.22,
                outer_angle: 0.6,
                cast_shadows: true,
                source_radius: None,
            }),
        ],
        materials: vec![LabMaterial {
            base_color: [0.78, 0.78, 0.8, 1.0],
            roughness: 0.55,
            ..LabMaterial::default()
        }],
        meshes: vec![
            LabMesh {
                vertices: floor_verts,
                indices: vec![0, 2, 1, 0, 3, 2],
                material: 0,
                ..LabMesh::unskinned()
            },
            sphere,
        ],
    }
}

/// Renders [`light_shadows_scene`].
pub fn render_light_shadows(device: &RenderDevice) -> Result<Vec<u8>, RenderError> {
    Ok(render_lab_scene(device, &light_shadows_scene())?.pixels)
}

fn rgba_rect(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 4]) -> LabTexture {
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            rgba.extend_from_slice(&pixel(x, y));
        }
    }
    LabTexture { width, height, rgba }
}

/// Normal-mapped plane with an emissive patch and an occlusion gradient.
///
/// The normal map carries continuous wave-shaped bumps (not a flat fill), so the
/// raking sun shows real normal-map detail across the whole plane.
pub fn surface_maps_scene() -> LabScene {
    let positions = [
        [-1.0, -1.0, 0.0],
        [1.0, -1.0, 0.0],
        [1.0, 1.0, 0.0],
        [-1.0, 1.0, 0.0],
    ];
    let normals = [[0.0, 0.0, 1.0]; 4];
    let uvs = [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]];
    let indices = vec![0, 1, 2, 0, 2, 3];
    let mut tangents = [[1.0, 0.0, 0.0, 1.0]; 4];
    generate_vertex_tangents(&positions, &normals, &uvs, &indices, &mut tangents);
    let vertices = (0..4)
        .map(|i| LabVertex {
            position: positions[i],
            normal: normals[i],
            uv: uvs[i],
            tangent: tangents[i],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect();
    // Height field h(x,y) = 0.5·sin(6πx)·cos(6πy); the normal map encodes
    // -∂h/∂x, -∂h/∂y (tangent space, +Z out of the surface).
    let normal_map = rgba_rect(128, 128, |x, y| {
        let u = x as f32 / 128.0;
        let v = y as f32 / 128.0;
        let dhdx = -(f32::cos(u * 6.0 * std::f32::consts::PI)
            * 0.5
            * 6.0
            * std::f32::consts::PI
            * f32::cos(v * 6.0 * std::f32::consts::PI))
            * 0.045;
        let dhdy = (f32::sin(u * 6.0 * std::f32::consts::PI)
            * f32::sin(v * 6.0 * std::f32::consts::PI)
            * 6.0
            * std::f32::consts::PI)
            * 0.045;
        let nx = (dhdx * 0.5 + 0.5).clamp(0.0, 1.0);
        let ny = (dhdy * 0.5 + 0.5).clamp(0.0, 1.0);
        [(nx * 255.0) as u8, (ny * 255.0) as u8, 255, 255]
    });
    let occlusion_map = rgba_rect(64, 64, |_x, y| {
        let ao = if y < 32 { 255 } else { 40 };
        [ao, ao, ao, 255]
    });
    let emissive_map = rgba_rect(64, 64, |x, y| {
        if (24..40).contains(&x) && (24..40).contains(&y) {
            [255, 180, 40, 255]
        } else {
            [0, 0, 0, 255]
        }
    });
    LabScene {
        width: GOLDEN_WIDTH,
        height: GOLDEN_HEIGHT,
        eye: [0.0, 0.35, 2.6],
        target: [0.0, 0.0, 0.0],
        fov_y: 45.0_f32.to_radians(),
        near: 0.1,
        far: 20.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: SUN_EV100,
        ambient_lux: 10_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([-0.6, -0.25, -1.0], [1.0, 1.0, 1.0], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.7, 0.72, 0.75, 1.0],
            roughness: 0.45,
            normal_scale: 1.0,
            occlusion_strength: 1.0,
            emissive: [1.0, 0.45, 0.05],
            emissive_intensity: 4.0,
            normal_map: Some(normal_map),
            occlusion_map: Some(occlusion_map),
            emissive_map: Some(emissive_map),
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices,
            indices,
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

/// Renders [`surface_maps_scene`].
pub fn render_surface_maps(device: &RenderDevice) -> Result<Vec<u8>, RenderError> {
    Ok(render_lab_scene(device, &surface_maps_scene())?.pixels)
}

fn xy_quad(x0: f32, x1: f32, y0: f32, y1: f32, z: f32) -> LabMesh {
    let corners = [
        ([x0, y0, z], [0.0, 1.0]),
        ([x1, y0, z], [1.0, 1.0]),
        ([x1, y1, z], [1.0, 0.0]),
        ([x0, y1, z], [0.0, 0.0]),
    ];
    LabMesh {
        vertices: corners
            .into_iter()
            .map(|(position, uv)| LabVertex {
                position,
                normal: [0.0, 0.0, 1.0],
                uv,
                tangent: [1.0, 0.0, 0.0, 1.0],
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            })
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material: 0,
        ..LabMesh::unskinned()
    }
}

/// Opaque, alpha-test and sorted blended quads.
pub fn alpha_modes_scene() -> LabScene {
    let mut opaque = xy_quad(-0.85, 0.15, -0.7, 0.7, 0.0);
    opaque.material = 0;
    let mut mask = xy_quad(-0.25, 0.75, -0.7, 0.7, 0.35);
    mask.material = 1;
    let mut additive = xy_quad(-0.05, 0.55, -0.15, 0.55, 0.55);
    additive.material = 2;
    let mut blend = xy_quad(-0.45, 0.95, -0.22, 0.22, 0.8);
    blend.material = 3;
    let checker = rgba_rect(4, 4, |x, y| {
        let on = (x + y) % 2 == 0;
        [255, 255, 255, if on { 255 } else { 0 }]
    });
    LabScene {
        width: GOLDEN_WIDTH,
        height: GOLDEN_HEIGHT,
        eye: [0.0, 0.0, 2.8],
        target: [0.0, 0.0, 0.0],
        fov_y: 45.0_f32.to_radians(),
        near: 0.1,
        far: 20.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: SUN_EV100,
        ambient_lux: 10_000.0,
        double_sided: false,
        post: None,
        directional: lab_directional_light(),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.9, 0.12, 0.08, 1.0],
                unlit: true,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.95, 0.95, 0.95, 1.0],
                alpha_mode: regia_render::ALPHA_MASK,
                alpha_cutoff: 0.5,
                unlit: true,
                albedo: Some(checker),
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.95, 0.75, 0.1, 1.0],
                alpha_mode: regia_render::ALPHA_ADDITIVE,
                unlit: true,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.15, 0.45, 0.95, 0.45],
                alpha_mode: regia_render::ALPHA_BLEND,
                unlit: true,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![opaque, mask, additive, blend],
    }
}

/// Renders [`alpha_modes_scene`].
pub fn render_alpha_modes(device: &RenderDevice) -> Result<Vec<u8>, RenderError> {
    Ok(render_lab_scene(device, &alpha_modes_scene())?.pixels)
}

/// Two spheres lit only by the procedural-sky IBL.
pub fn ibl_spheres_scene() -> LabScene {
    let mut metal = lab_sphere([-0.55, 0.0, 0.0], 0.42);
    metal.material = 0;
    let mut dielectric = lab_sphere([0.55, 0.0, 0.0], 0.42);
    dielectric.material = 1;
    let mut sun = lab_directional_light();
    sun.illuminance = 0.0;
    LabScene {
        width: GOLDEN_WIDTH,
        height: GOLDEN_HEIGHT,
        eye: [0.0, 0.3, 1.85],
        target: [0.0, 0.0, 0.0],
        fov_y: 40.0_f32.to_radians(),
        near: 0.1,
        far: 20.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        // Metered for the sky the spheres reflect (horizon ≈ 7 000 cd/m²). It was 12.9,
        // 1.1 EV brighter, to lift mid-tones the old linear-input AgX fit printed at 0.17;
        // the frame now also shows that sky behind the spheres.
        exposure_ev: 14.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: None,
        directional: sun,
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.95, 0.95, 0.97, 1.0],
                metallic: 1.0,
                roughness: 0.15,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.75, 0.25, 0.15, 1.0],
                metallic: 0.0,
                roughness: 0.65,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![metal, dielectric],
    }
}

/// Renders [`ibl_spheres_scene`].
pub fn render_ibl_spheres(device: &RenderDevice) -> Result<Vec<u8>, RenderError> {
    Ok(render_lab_scene(device, &ibl_spheres_scene())?.pixels)
}

/// Three spheres used by the post-stack golden (glow, grey, red).
pub fn post_demo_scene(post: Option<PostSettings>) -> LabScene {
    let mut glow = lab_sphere([0.0, 0.08, 0.0], 0.2);
    glow.material = 0;
    let mut grey = lab_sphere([0.62, -0.08, 0.2], 0.26);
    grey.material = 1;
    let mut red = lab_sphere([-0.62, -0.08, 0.2], 0.26);
    red.material = 2;
    LabScene {
        width: GOLDEN_WIDTH,
        height: GOLDEN_HEIGHT,
        eye: [0.0, 0.12, 1.95],
        target: [0.0, 0.0, 0.0],
        fov_y: 40.0_f32.to_radians(),
        near: 0.1,
        far: 20.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: SUN_EV100,
        ambient_lux: 8_000.0,
        double_sided: false,
        post,
        directional: DirectionalLightUniform::new([0.2, -0.4, -1.0], [1.0, 1.0, 1.0], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.02, 0.02, 0.025, 1.0],
                emissive: [1.0, 0.72, 0.25],
                // A bright lamp in nits: ~2·10⁵ cd/m² so it stays the dominant
                // light after EV100 exposure and drives the bloom.
                emissive_intensity: 200_000.0,
                roughness: 0.4,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.45, 0.48, 0.52, 1.0],
                roughness: 0.35,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.62, 0.12, 0.08, 1.0],
                roughness: 0.45,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![glow, grey, red],
    }
}

/// Post stack on (right half) and off (left half): bloom, AgX, vignette, FXAA.
pub fn render_post_stack(device: &RenderDevice) -> Result<Vec<u8>, RenderError> {
    let off = render_lab_scene(device, &post_demo_scene(None))?.pixels;
    let on = PostSettings {
        enabled: true,
        bloom: true,
        bloom_mips: 4,
        auto_exposure: false,
        exposure_ev: SUN_EV100,
        tonemap: TonemapOperator::AgX,
        fxaa: true,
        taa: false,
        vignette: 0.55,
        film_grain: 0.0,
        ..PostSettings::default()
    };
    let on_pixels = render_lab_scene(device, &post_demo_scene(Some(on)))?.pixels;
    let mut out = vec![0u8; (GOLDEN_WIDTH * GOLDEN_HEIGHT * 4) as usize];
    let half = GOLDEN_WIDTH / 2;
    for y in 0..GOLDEN_HEIGHT {
        for x in 0..GOLDEN_WIDTH {
            let src = if x < half { &off } else { &on_pixels };
            let index = ((y * GOLDEN_WIDTH + x) * 4) as usize;
            out[index..index + 4].copy_from_slice(&src[index..index + 4]);
        }
    }
    Ok(out)
}

/// Width of the primitives row golden.
pub const PRIMITIVES_ROW_WIDTH: u32 = 704;
/// Height of the primitives row golden.
pub const PRIMITIVES_ROW_HEIGHT: u32 = 224;
/// Centre-to-centre spacing of the row, metres.
const PRIMITIVE_SPACING: f32 = 1.45;

/// Distinct mid-saturation colours, one per shape.
const PRIMITIVE_PALETTE: [[f32; 4]; 11] = [
    [0.70, 0.70, 0.72, 1.0],
    [0.80, 0.25, 0.20, 1.0],
    [0.85, 0.55, 0.15, 1.0],
    [0.80, 0.75, 0.20, 1.0],
    [0.35, 0.70, 0.25, 1.0],
    [0.20, 0.60, 0.55, 1.0],
    [0.25, 0.45, 0.80, 1.0],
    [0.45, 0.30, 0.80, 1.0],
    [0.75, 0.30, 0.65, 1.0],
    [0.55, 0.45, 0.35, 1.0],
    [0.90, 0.90, 0.88, 1.0],
];

/// The row's shapes: the defaults, with the plane shrunk to a tile and the bigger shapes trimmed
/// to fit a 1.45 m slot.
fn primitive_row_shapes() -> Vec<PrimitiveShape> {
    PrimitiveShape::KINDS
        .iter()
        .map(|kind| match *kind {
            "plane" => PrimitiveShape::Plane {
                size: Vec2::splat(1.1),
                subdivisions: 0,
            },
            "ramp" => PrimitiveShape::Ramp {
                size: Vec3::new(1.2, 0.8, 1.0),
            },
            "capsule" => PrimitiveShape::Capsule {
                radius: 0.4,
                length: 0.6,
                longitudes: 32,
                latitudes: 16,
            },
            "torus" => PrimitiveShape::Torus {
                minor_radius: 0.15,
                major_radius: 0.45,
                minor_resolution: 24,
                major_resolution: 32,
            },
            other => PrimitiveShape::default_of(other).expect("known kind"),
        })
        .collect()
}

/// Every greybox primitive in one row on a floor, sun + sky lit (the `primitives_row` golden).
#[must_use]
pub fn primitives_row_scene() -> LabScene {
    let shapes = primitive_row_shapes();
    let offset = (shapes.len() as f32 - 1.0) * PRIMITIVE_SPACING * 0.5;
    let mut meshes = Vec::new();
    let mut materials = Vec::new();

    // Dark floor slab under the whole row.
    let (floor_v, floor_i) = box_mesh([offset + 1.5, 0.05, 2.0]);
    materials.push(LabMaterial {
        base_color: [0.18, 0.18, 0.19, 1.0],
        roughness: 0.8,
        ..LabMaterial::default()
    });
    meshes.push(LabMesh {
        vertices: floor_v,
        indices: floor_i,
        material: 0,
        model: Mat4::from_translation(Vec3::new(0.0, -0.05, 0.0)).to_cols_array_2d(),
        ..LabMesh::unskinned()
    });

    for (i, shape) in shapes.iter().enumerate() {
        let mesh = shape.mesh().expect("valid row shape");
        // Rest each shape on the floor; turn the torus up so its ring reads, and the ramp and
        // prism a little so their caps show.
        let rotation = match shape.kind() {
            "torus" => Quat::from_rotation_x(-1.1),
            "ramp" | "prism" => Quat::from_rotation_y(0.5),
            _ => Quat::IDENTITY,
        };
        let lowest = mesh
            .vertices
            .iter()
            .map(|v| (rotation * Vec3::from_array(v.position)).y)
            .fold(f32::MAX, f32::min);
        let lift = if shape.kind() == "plane" { 0.002 } else { -lowest };
        let model = Mat4::from_rotation_translation(
            rotation,
            Vec3::new(i as f32 * PRIMITIVE_SPACING - offset, lift, 0.0),
        );
        materials.push(LabMaterial {
            base_color: PRIMITIVE_PALETTE[i % PRIMITIVE_PALETTE.len()],
            roughness: 0.55,
            ..LabMaterial::default()
        });
        meshes.push(LabMesh {
            vertices: mesh.vertices,
            indices: mesh.indices,
            material: materials.len() - 1,
            model: model.to_cols_array_2d(),
            ..LabMesh::unskinned()
        });
    }

    LabScene {
        width: PRIMITIVES_ROW_WIDTH,
        height: PRIMITIVES_ROW_HEIGHT,
        eye: [0.0, 4.2, 12.5],
        target: [0.0, 0.45, 0.0],
        fov_y: 22.0_f32.to_radians(),
        near: 0.1,
        far: 60.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: SUN_EV100,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: None,
        directional: lab_directional_light(),
        local_lights: Vec::new(),
        materials,
        meshes,
    }
}
