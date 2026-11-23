// SPDX-License-Identifier: MPL-2.0
//! Visible-light budget, a shadowed spot, and an enclosed room.
//!
//! The 200-light case is the DO27 tunnel. Desktop High uploads all of them.
//! The camera sits on a light past the old 48-prefix, so that prefix cannot
//! reach the floor and the full list must. Every light landing in a froxel is
//! the `light_budget` unit test (a side view leaves the ends outside the frustum).

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, GiMode, INTERIOR_AMBIENT_FLOOR_LUX,
    INTERIOR_EXPOSURE_EV, InteriorVolume, LabLocalLight, LabMaterial, LabMesh, LabPointLight, LabScene,
    LabSpotLight, LabVertex, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SdfScene,
    box_mesh, build_interior_lighting, hollow_box,
};

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip local lights: {err}");
            None
        }
    }
}

fn region_luma(pixels: &[u8], width: u32, x0: u32, y0: u32, x1: u32, y1: u32) -> f32 {
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * width + x) * 4) as usize;
            sum += f32::from(pixels[i]) + f32::from(pixels[i + 1]) + f32::from(pixels[i + 2]);
            n += 1.0;
        }
    }
    if n > 0.0 { sum / n } else { 0.0 }
}

/// BT.709 luma, 0–255. A region near 1 is still black on screen.
fn bt709(pixels: &[u8], width: u32, x0: u32, y0: u32, x1: u32, y1: u32) -> f32 {
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * width + x) * 4) as usize;
            let r = f32::from(pixels[i]);
            let g = f32::from(pixels[i + 1]);
            let b = f32::from(pixels[i + 2]);
            sum += 0.2126 * r + 0.7152 * g + 0.0722 * b;
            n += 1.0;
        }
    }
    if n > 0.0 { sum / n } else { 0.0 }
}

fn quad(min: [f32; 3], max: [f32; 3], normal: [f32; 3]) -> LabMesh {
    let vertices = [
        [min[0], min[1], min[2]],
        [max[0], min[1], min[2]],
        [max[0], max[1], max[2]],
        [min[0], max[1], max[2]],
    ]
    .into_iter()
    .map(|position| LabVertex::from_pos_normal_uv(position, normal, [0.0, 0.0]))
    .collect();
    LabMesh {
        vertices,
        indices: vec![0, 2, 1, 0, 3, 2],
        material: 0,
        ..LabMesh::unskinned()
    }
}

fn grey() -> LabMaterial {
    LabMaterial {
        base_color: [0.7, 0.7, 0.68, 1.0],
        roughness: 0.7,
        ..LabMaterial::default()
    }
}

fn sun_off() -> DirectionalLightUniform {
    DirectionalLightUniform::new([0.2, -1.0, 0.15], [1.0, 1.0, 1.0], 0.0)
}

fn desktop_high_cheap() -> FrameQuality {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.local_shadow_atlas_size = 0;
    quality.local_shadow_max_lights = 0;
    quality.csm_cascade_count = 1;
    quality.csm_resolution = 64;
    quality.gpu_driven = false;
    quality.ssao = regia_render::SsaoMode::Off;
    quality.ssr = false;
    quality.screen_space_gi = false;
    quality.gi = regia_render::GiMode::Ambient;
    quality.reflection_probes = false;
    quality.paged_sun_shadows = false;
    quality.taau = false;
    quality.horizontal_plus = false;
    quality
}

fn render(scene: &LabScene, quality: FrameQuality) -> (Vec<u8>, regia_render::LightUploadReport) {
    let device = device().expect("caller checks the device");
    let mut renderer = FrameRenderer::new();
    let pixels = renderer
        .render_offscreen(&device, &scene.as_frame_scene_with_quality(quality))
        .expect("render")
        .pixels;
    (pixels, renderer.light_upload())
}

/// The enclosed-room path: [`build_interior_lighting`] plus one reflection probe.
fn render_enclosed(
    scene: &LabScene,
    mut quality: FrameQuality,
    volume: &InteriorVolume,
    sdf: &SdfScene,
) -> Vec<u8> {
    let device = device().expect("caller checks the device");
    let lighting =
        build_interior_lighting(volume, sdf, &scene.local_lights, 0.9, 4.0, 20).expect("interior field");
    assert!(
        lighting.coverage.enclosed >= 1,
        "the room must keep an enclosed probe, {:?}",
        lighting.coverage
    );
    quality.gi = GiMode::Probes;
    quality.reflection_probes = true;
    quality.ssr = false;
    quality.max_reflection_probes = quality.max_reflection_probes.max(1);
    let mut scene = scene.clone();
    scene.depth_prepass = true;
    let mut post = PostSettings::golden_deterministic();
    post.exposure_ev = scene.exposure_ev;
    scene.post = Some(post);
    let mut renderer = FrameRenderer::new();
    renderer.set_gi_field(Some(lighting.field));
    let frame = scene.as_frame_scene_with_quality(quality);
    let probe = renderer
        .capture_reflection_probe(
            &device,
            &frame,
            1,
            lighting.coverage.reflection_center,
            lighting.coverage.reflection_half_extent,
        )
        .expect("reflection probe");
    let seen = probe
        .equirect
        .chunks(4)
        .filter(|texel| texel[0] > 8 || texel[1] > 8 || texel[2] > 8)
        .count();
    assert!(
        seen > 8,
        "the reflection probe must see the lit room, texels {seen}"
    );
    renderer.set_reflection_probes(vec![probe]);
    renderer
        .render_offscreen(&device, &scene.as_frame_scene_with_quality(quality))
        .expect("render")
        .pixels
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn two_hundred_lights_all_upload_and_the_ones_past_forty_eight_light_the_floor() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if device().is_none() {
        return;
    }
    let quality = desktop_high_cheap();
    assert!(quality.max_visible_lights >= 256);
    // 6 m spacing, 10 m radius. Light 47 sits at x = 282. The camera looks at
    // light 90 (x = 540), which the old prefix cannot reach. 12 klm at EV 4
    // clears the AgX toe; 600 lm at EV 6 on a side view did not.
    let lights: Vec<LabLocalLight> = (0..200)
        .map(|index| {
            LabLocalLight::Point(regia_render::LabPointLight {
                position: [index as f32 * 6.0, 2.4, 0.0],
                color: [1.0, 0.92, 0.78],
                intensity: 12_000.0,
                radius: 10.0,
                cast_shadows: false,
                source_radius: None,
            })
        })
        .collect();
    let mut scene = LabScene {
        width: 160,
        height: 90,
        eye: [540.0, 4.2, 7.0],
        target: [540.0, 0.2, 0.0],
        fov_y: 55.0_f32.to_radians(),
        near: 0.15,
        far: 40.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 4.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: None,
        directional: sun_off(),
        local_lights: lights,
        materials: vec![grey()],
        meshes: vec![quad([500.0, 0.0, -10.0], [580.0, 0.0, 10.0], [0.0, 1.0, 0.0])],
    };
    let (full, report) = render(&scene, quality);
    assert_eq!(report.authored, 200);
    assert_eq!(report.uploaded, 200, "Desktop High must not stop at 48 or 64");
    assert_eq!(report.dropped, 0);
    assert_eq!(
        report.cluster_overflow, 0,
        "a spread of 200 lights must not fill a Desktop High froxel"
    );
    scene.local_lights.truncate(48);
    let (prefix, prefix_report) = render(&scene, quality);
    assert_eq!(prefix_report.uploaded, 48);
    // The lit disk is the lower half of the frame. Whole-frame means mix in the
    // clear colour and hide a real contribution.
    let full_l = region_luma(&full, 160, 40, 40, 120, 85);
    let prefix_l = region_luma(&prefix, 160, 40, 40, 120, 85);
    // Sum of channels. A luma-~3 grey is about 9 and must fail; the lit floor is far above.
    assert!(
        full_l > 24.0,
        "light 90 must light the floor under the camera, luma {full_l}"
    );
    assert!(
        full_l > prefix_l + 4.0,
        "lights past the old prefix must reach the floor (full {full_l}, prefix {prefix_l})"
    );
}

#[test]
fn a_spot_shadow_darkens_the_floor_under_a_mesh() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if device().is_none() {
        return;
    }
    let (box_verts, box_idx) = box_mesh([0.55, 0.75, 0.55]);
    let floor = quad([-3.0, 0.0, -3.0], [3.0, 0.0, 3.0], [0.0, 1.0, 0.0]);
    let caster = LabMesh {
        vertices: box_verts,
        indices: box_idx,
        material: 0,
        model: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.75, 0.0, 1.0],
        ],
        ..LabMesh::unskinned()
    };
    // 4 kcd at EV 4. 180 cd at EV 8 landed under the AgX toe (a few texels of
    // dark red, identical with and without the shadow).
    let spot = |shadows: bool| {
        LabLocalLight::Spot(LabSpotLight {
            position: [-1.5, 2.2, 1.1],
            direction: [0.85, -0.9, -0.45],
            color: [1.0, 0.95, 0.85],
            intensity: 4_000.0,
            range: 12.0,
            inner_angle: 0.2,
            outer_angle: 0.65,
            cast_shadows: shadows,
            source_radius: None,
        })
    };
    let scene = |shadows: bool| LabScene {
        width: 160,
        height: 120,
        eye: [2.8, 2.4, 2.6],
        target: [0.4, 0.05, 0.15],
        fov_y: 50.0_f32.to_radians(),
        near: 0.1,
        far: 30.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 4.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: None,
        directional: sun_off(),
        local_lights: vec![spot(shadows)],
        materials: vec![grey()],
        meshes: vec![floor.clone(), caster.clone()],
    };
    let (lit, lit_report) = render(&scene(false), FrameQuality::LAB);
    let (shaded, shade_report) = render(&scene(true), FrameQuality::LAB);
    assert_eq!(lit_report.uploaded, 1);
    assert_eq!(shade_report.uploaded, 1);
    assert_eq!(shade_report.dropped, 0);
    // Floor in the lower middle, where the beam passes the box.
    let lit_l = region_luma(&lit, 160, 50, 55, 120, 110);
    let shaded_l = region_luma(&shaded, 160, 50, 55, 120, 110);
    // Sum of channels. A luma-~3 grey is about 9 and must fail.
    assert!(lit_l > 24.0, "the spot must light the floor, luma {lit_l}");
    assert!(
        shaded_l + 1.0 < lit_l,
        "the mesh must cast a spot shadow (lit {lit_l}, shaded {shaded_l})"
    );
}

/// v0.1.16 (ADR-223): a shadow-casting light under a zero light cap (DO27's Low sets
/// `local_shadow_max_lights: 0`) allocates no local shadow atlas: before, the 4096² depth atlas
/// (64 MiB) was created with no tile in it. With a non-zero cap the atlas is allocated as before.
#[test]
fn casting_lights_under_a_zero_light_cap_allocate_no_local_atlas() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let floor = quad([-3.0, 0.0, -3.0], [3.0, 0.0, 3.0], [0.0, 1.0, 0.0]);
    let (box_verts, box_idx) = box_mesh([0.55, 0.75, 0.55]);
    let caster = LabMesh {
        vertices: box_verts,
        indices: box_idx,
        material: 0,
        ..LabMesh::unskinned()
    };
    let scene = LabScene {
        width: 64,
        height: 48,
        eye: [2.8, 2.4, 2.6],
        target: [0.4, 0.05, 0.15],
        fov_y: 50.0_f32.to_radians(),
        near: 0.1,
        far: 30.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 4.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: None,
        directional: sun_off(),
        local_lights: vec![LabLocalLight::Spot(LabSpotLight {
            position: [-1.5, 2.2, 1.1],
            direction: [0.85, -0.9, -0.45],
            color: [1.0, 0.95, 0.85],
            intensity: 4_000.0,
            range: 12.0,
            inner_angle: 0.2,
            outer_angle: 0.65,
            cast_shadows: true,
            source_radius: None,
        })],
        materials: vec![grey()],
        meshes: vec![floor, caster],
    };
    let atlas_bytes = |max_lights: u32| {
        let mut quality = FrameQuality::LAB;
        quality.local_shadow_atlas_size = 4096;
        quality.local_shadow_max_lights = max_lights;
        let mut renderer = FrameRenderer::new();
        renderer
            .render_offscreen(&device, &scene.as_frame_scene_with_quality(quality))
            .expect("render");
        let report = regia_render::memory_report(&device);
        if !report.available {
            // Only Vulkan and DX12 expose wgpu's allocator report (Metal does not).
            return None;
        }
        let bytes: u64 = report
            .labels
            .iter()
            .filter(|l| l.label == "shadow atlas")
            .map(|l| l.bytes)
            .sum();
        drop(renderer);
        Some(bytes)
    };
    let Some(capped) = atlas_bytes(0) else {
        eprintln!("skipping: no allocator report on this backend");
        return;
    };
    assert!(
        capped < 1 << 20,
        "no atlas without a light to shadow: {capped} bytes"
    );
    let shadowed = atlas_bytes(8).expect("the report was available for the capped run");
    assert!(
        shadowed >= 4096 * 4096 * 4,
        "a shadowed light keeps its 4096² atlas: {shadowed} bytes"
    );
}

#[test]
fn an_enclosed_room_lit_only_by_local_lights_has_no_black_facing_surface() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if device().is_none() {
        return;
    }
    let (shell_v, shell_i) = box_mesh([2.2, 1.3, 3.0]);
    let shell = LabMesh {
        vertices: shell_v,
        indices: shell_i,
        material: 0,
        ..LabMesh::unskinned()
    };
    let light = LabLocalLight::Point(LabPointLight {
        position: [0.0, 0.35, 0.2],
        color: [1.0, 0.72, 0.45],
        intensity: 6_000.0,
        radius: 6.0,
        cast_shadows: false,
        source_radius: None,
    });
    let mut material = grey();
    material.roughness = 0.5;
    let mut quality = FrameQuality::LAB;
    quality.interior_ambient = true;
    quality.local_shadow_atlas_size = 0;
    let scene = LabScene {
        width: 128,
        height: 96,
        eye: [0.0, 0.35, 1.2],
        target: [0.0, 0.4, -1.0],
        fov_y: 70.0_f32.to_radians(),
        near: 0.05,
        far: 20.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: INTERIOR_EXPOSURE_EV,
        ambient_lux: INTERIOR_AMBIENT_FLOOR_LUX,
        double_sided: true,
        post: None,
        directional: sun_off(),
        local_lights: vec![light],
        materials: vec![material],
        meshes: vec![shell],
    };
    let volume = InteriorVolume::with_floor([0.0, 0.0, 0.0], [1.9, 1.05, 2.6]);
    let sdf = hollow_box([0.0, 0.0, 0.0], [2.2, 1.3, 3.0], 0.08, [0.7, 0.7, 0.68]);
    let lit = render_enclosed(&scene, quality, &volume, &sdf);
    let center = bt709(&lit, 128, 40, 30, 88, 66);
    // Luma 1 is still black. The 40 lux floor alone is near 30; the facing wall is above that.
    assert!(
        center > 16.0,
        "the wall facing the only local light must clear luma 1, bt709 {center}"
    );
    let mut direct = scene.clone();
    let mut direct_post = PostSettings::golden_deterministic();
    direct_post.exposure_ev = direct.exposure_ev;
    direct.post = Some(direct_post);
    let (direct_px, report) = render(&direct, quality);
    assert_eq!(report.uploaded, 1);
    let direct_l = bt709(&direct_px, 128, 40, 30, 88, 66);
    assert!(
        center > direct_l + 1.5,
        "the interior field must add bounce on the facing wall (with {center}, direct {direct_l})"
    );
    let mut dark = scene.clone();
    dark.local_lights.clear();
    dark.ambient_lux = INTERIOR_AMBIENT_FLOOR_LUX;
    let floor_only = render_enclosed(&dark, quality, &volume, &sdf);
    let floor_l = bt709(&floor_only, 128, 40, 30, 88, 66);
    assert!(
        floor_l > 16.0,
        "40 lux at EV {INTERIOR_EXPOSURE_EV} must clear luma 1, bt709 {floor_l}"
    );
    assert!(
        center > floor_l + 8.0,
        "the local light must lift the wall above the ambient floor (lit {center}, floor {floor_l})"
    );
    let mut crushed = quality;
    crushed.interior_ambient = false;
    dark.ambient_lux = 0.0;
    dark.exposure_ev = 15.0;
    dark.post = None;
    let (black, _) = render(&dark, crushed);
    let black_l = bt709(&black, 128, 40, 30, 88, 66);
    assert!(
        black_l < floor_l,
        "without the floor the same wall is darker (floor {floor_l}, bare {black_l})"
    );
}

fn bt709_samples(pixels: &[u8], width: u32, x0: u32, y0: u32, x1: u32, y1: u32) -> Vec<f32> {
    let mut values = Vec::new();
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * width + x) * 4) as usize;
            let Some(px) = pixels.get(i..i + 3) else {
                continue;
            };
            values.push(0.2126 * f32::from(px[0]) + 0.7152 * f32::from(px[1]) + 0.0722 * f32::from(px[2]));
        }
    }
    values
}

fn min_median(values: &[f32]) -> (f32, f32) {
    if values.is_empty() {
        return (0.0, 0.0);
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    (sorted[0], sorted[sorted.len() / 2])
}

/// A dark texel whose neighbourhood is lit. A real shadow has dark neighbours.
fn isolated_dark_texels(pixels: &[u8], width: u32, height: u32) -> u32 {
    let luma = |x: u32, y: u32| -> f32 {
        let i = ((y * width + x) * 4) as usize;
        0.2126 * f32::from(pixels[i]) + 0.7152 * f32::from(pixels[i + 1]) + 0.0722 * f32::from(pixels[i + 2])
    };
    if width < 3 || height < 3 || pixels.len() < (width * height * 4) as usize {
        return 0;
    }
    let mut count = 0u32;
    for y in 1..height - 1 {
        for x in 1..width - 1 {
            if luma(x, y) > 8.0 {
                continue;
            }
            let mut bright = 0u32;
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let nx = (x as i32 + dx) as u32;
                    let ny = (y as i32 + dy) as u32;
                    if luma(nx, ny) >= 40.0 {
                        bright += 1;
                    }
                }
            }
            if bright >= 6 {
                count += 1;
            }
        }
    }
    count
}

fn plane(corners: [[f32; 3]; 4], normal: [f32; 3]) -> LabMesh {
    let vertices = corners
        .into_iter()
        .map(|position| LabVertex::from_pos_normal_uv(position, normal, [0.0, 0.0]))
        .collect();
    LabMesh {
        vertices,
        indices: vec![0, 1, 2, 0, 2, 3],
        material: 0,
        ..LabMesh::unskinned()
    }
}

fn dump_ppm(name: &str, width: u32, height: u32, pixels: &[u8]) {
    let Ok(dir) = regia_core::brand::env_var("REGIA_SHADOW_DEBUG") else {
        return;
    };
    let path = std::path::Path::new(&dir).join(name);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    for px in pixels.chunks(4) {
        if px.len() < 3 {
            break;
        }
        rgb.extend_from_slice(&px[..3]);
    }
    let header = format!("P6\n{width} {height}\n255\n");
    let mut bytes = header.into_bytes();
    bytes.extend_from_slice(&rgb);
    let _ = std::fs::write(path, bytes);
}

#[test]
fn an_unoccluded_wall_at_a_tile_border_has_no_dark_blotch() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if device().is_none() {
        return;
    }
    // The spot looks down, so world −X lands on the right edge of its atlas
    // tile — the edge packed against the next light. That neighbour's map is
    // full of near depths. Nothing stands between the spot and this wall.
    let wall = plane(
        [
            [-1.15, 0.0, -1.2],
            [-1.15, 0.0, 1.2],
            [-1.15, 2.2, 1.2],
            [-1.15, 2.2, -1.2],
        ],
        [1.0, 0.0, 0.0],
    );
    let floor = quad([-2.0, 0.0, -2.0], [1.5, 0.0, 2.0], [0.0, 1.0, 0.0]);
    let (box_v, box_i) = box_mesh([0.35, 0.2, 0.35]);
    let blocker = LabMesh {
        vertices: box_v,
        indices: box_i,
        material: 0,
        model: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [4.0, 0.25, 0.0, 1.0],
        ],
        ..LabMesh::unskinned()
    };
    let wall_spot = LabLocalLight::Spot(LabSpotLight {
        position: [0.0, 2.05, 0.0],
        direction: [0.0, -1.0, 0.0],
        color: [1.0, 0.95, 0.88],
        intensity: 8_000.0,
        range: 8.0,
        inner_angle: 0.35,
        outer_angle: 0.78,
        cast_shadows: true,
        source_radius: None,
    });
    let neighbor = |shadows: bool| {
        LabLocalLight::Spot(LabSpotLight {
            position: [4.0, 0.7, 0.0],
            direction: [0.0, -1.0, 0.0],
            color: [1.0, 1.0, 1.0],
            intensity: 200.0,
            range: 1.2,
            inner_angle: 0.2,
            outer_angle: 0.6,
            cast_shadows: shadows,
            source_radius: None,
        })
    };
    let scene = |shadows: bool| LabScene {
        width: 160,
        height: 120,
        eye: [-2.6, 1.15, 1.7],
        target: [-1.15, 0.85, 0.0],
        fov_y: 48.0_f32.to_radians(),
        near: 0.08,
        far: 20.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 4.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: None,
        directional: sun_off(),
        local_lights: vec![wall_spot, neighbor(shadows)],
        materials: vec![grey()],
        meshes: vec![floor.clone(), wall.clone(), blocker.clone()],
    };
    let (alone, _) = render(&scene(false), FrameQuality::LAB);
    let (with_neighbor, _) = render(&scene(true), FrameQuality::LAB);
    dump_ppm("wall_alone.ppm", 160, 120, &alone);
    dump_ppm("wall_neighbor.ppm", 160, 120, &with_neighbor);
    // The bright interior of the cone on the wall. The cone edge and the
    // black surround are unlit, not a shadow blotch.
    let crop = |pixels: &[u8]| bt709_samples(pixels, 160, 72, 76, 106, 94);
    let (alone_min, alone_med) = min_median(&crop(&alone));
    let (near_min, near_med) = min_median(&crop(&with_neighbor));
    assert!(
        alone_med > 20.0,
        "the unoccluded wall must be lit, median {alone_med} min {alone_min}"
    );
    assert!(
        near_med > alone_med * 0.75,
        "the neighbor tile bled into the wall (alone {alone_med}, with neighbor {near_med})"
    );
    assert!(
        near_min > near_med * 0.65,
        "dark blotch on the unoccluded wall (min {near_min}, median {near_med})"
    );
    assert_eq!(
        isolated_dark_texels(&with_neighbor, 160, 120),
        0,
        "lit wall has isolated black texels"
    );
}

#[test]
fn a_point_light_cube_shadow_darkens_only_the_occluded_side() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if device().is_none() {
        return;
    }
    let wall = plane(
        [
            [-2.0, 0.2, 0.0],
            [2.0, 0.2, 0.0],
            [2.0, 2.8, 0.0],
            [-2.0, 2.8, 0.0],
        ],
        [0.0, 0.0, 1.0],
    );
    let (box_v, box_i) = box_mesh([0.4, 0.45, 0.18]);
    let blocker = LabMesh {
        vertices: box_v,
        indices: box_i,
        material: 0,
        model: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 1.5, 0.85, 1.0],
        ],
        ..LabMesh::unskinned()
    };
    let scene = LabScene {
        width: 160,
        height: 120,
        eye: [0.0, 1.55, 3.1],
        target: [0.0, 1.45, 0.0],
        fov_y: 50.0_f32.to_radians(),
        near: 0.08,
        far: 20.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 4.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: None,
        directional: sun_off(),
        local_lights: vec![LabLocalLight::Point(LabPointLight {
            position: [0.0, 1.5, 1.7],
            color: [1.0, 0.96, 0.9],
            intensity: 9_000.0,
            radius: 8.0,
            cast_shadows: true,
            source_radius: None,
        })],
        materials: vec![grey()],
        meshes: vec![wall, blocker],
    };
    let (shaded, report) = render(&scene, FrameQuality::LAB);
    dump_ppm("point_cube.ppm", 160, 120, &shaded);
    assert_eq!(report.uploaded, 1);
    // The light is between the camera and the box, so the umbra on the wall is
    // larger than the box and shows beside it. The frame centre is the lit
    // front face of the box. The side crop includes the cube-face seam.
    let side_samples = bt709_samples(&shaded, 160, 4, 36, 28, 68);
    let (side_min, side_med) = min_median(&side_samples);
    let umbra = bt709(&shaded, 160, 42, 48, 52, 72);
    assert!(
        side_med > 20.0,
        "the unoccluded side of the wall must stay lit, median {side_med}"
    );
    assert!(
        side_min > side_med * 0.75,
        "the unoccluded side has a dark blotch (min {side_min}, median {side_med})"
    );
    assert!(
        umbra + 40.0 < side_med,
        "the box must shadow the wall beside it (umbra {umbra}, side {side_med})"
    );
    assert_eq!(isolated_dark_texels(&shaded, 160, 120), 0, "cube shadow acne");
}
