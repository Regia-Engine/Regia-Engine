// SPDX-License-Identifier: MPL-2.0
//! The precipitation occlusion view (spec 99 § 11): a tile of the shadow atlas seen from the sky,
//! drawn when it changes and held otherwise, on paged and unpaged frames; the read-back map says
//! what is covered.

use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    LabVertex, RenderDevice, SurfaceWeatherFrame,
};
use regia_test_utils::serial_gpu::{SerialGpu, serial_device};

const W: u32 = 64;
const H: u32 = 64;

/// The binary's one shared device, one test at a time: per-test devices on parallel threads hung
/// the NVIDIA driver (RTX 5090 GPU pre-gate, ADR-7483).
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

/// A floor and a roof slab over x in -6..-1 at 3 m.
fn scene(eye: [f32; 3]) -> LabScene {
    let (floor_v, floor_i) = quad(
        [
            [-40.0, 0.0, -40.0],
            [40.0, 0.0, -40.0],
            [40.0, 0.0, 40.0],
            [-40.0, 0.0, 40.0],
        ],
        [0.0, 1.0, 0.0],
    );
    let (roof_v, roof_i) = quad(
        [
            [-6.0, 3.0, -3.0],
            [-1.0, 3.0, -3.0],
            [-1.0, 3.0, 3.0],
            [-6.0, 3.0, 3.0],
        ],
        [0.0, -1.0, 0.0],
    );
    LabScene {
        width: W,
        height: H,
        eye,
        target: [eye[0], 0.0, eye[2] - 8.0],
        fov_y: 0.9,
        near: 0.1,
        far: 80.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 8_000.0,
        double_sided: true,
        post: None,
        directional: DirectionalLightUniform::new([0.3, -1.0, -0.4], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial::default()],
        meshes: vec![
            LabMesh {
                vertices: floor_v,
                indices: floor_i,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: roof_v,
                indices: roof_i,
                ..LabMesh::unskinned()
            },
        ],
    }
}

const WET: SurfaceWeatherFrame = SurfaceWeatherFrame {
    wetness: 1.0,
    puddles: 0.0,
    snow_cover: 0.0,
    ripple: 0.0,
    clock_s: 0.0,
};

fn renderer() -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_surface_weather(WET);
    renderer.set_precipitation_occlusion_budget(Some((256, 32.0)), 16_384);
    renderer
}

fn frame(renderer: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene, quality: FrameQuality) {
    if let Err(err) = renderer.render_offscreen(device, &lab.as_frame_scene_with_quality(quality)) {
        panic!("frame: {err}");
    }
}

fn paged() -> FrameQuality {
    FrameQuality {
        paged_sun_shadows: true,
        ..FrameQuality::LAB
    }
}

fn run_redraw_rules(quality: FrameQuality) {
    let device = device();
    let lab = scene([0.0, 5.0, 8.0]);
    let mut renderer = renderer();
    frame(&mut renderer, &device, &lab, quality);
    let info = renderer.precipitation_occlusion().expect("on");
    assert_eq!(info.redraws, 1, "the first frame draws it");
    assert_eq!((info.size_texels, info.extent_m), (256, 32.0));
    assert_eq!(info.tilt_deg, 0.0);
    frame(&mut renderer, &device, &lab, quality);
    frame(&mut renderer, &device, &lab, quality);
    assert_eq!(
        renderer.precipitation_occlusion().expect("on").redraws,
        1,
        "a held frame draws nothing"
    );
    // Under a snap (32 m / 8 = 4 m): held.
    frame(&mut renderer, &device, &scene([0.5, 5.0, 8.0]), quality);
    assert_eq!(renderer.precipitation_occlusion().expect("on").redraws, 1);
    // A whole snap away: drawn once more, then held.
    let moved = scene([9.0, 5.0, 8.0]);
    frame(&mut renderer, &device, &moved, quality);
    frame(&mut renderer, &device, &moved, quality);
    assert_eq!(renderer.precipitation_occlusion().expect("on").redraws, 2);
}

#[test]
fn the_tile_redraws_on_a_snap_and_not_on_a_held_frame_when_unpaged() {
    run_redraw_rules(FrameQuality::LAB);
}

#[test]
fn the_tile_redraws_on_a_snap_and_not_on_a_held_frame_when_paged() {
    run_redraw_rules(paged());
}

/// A caster moving over the square redraws the tile; off the square it does not.
#[test]
fn a_moving_caster_redraws_the_tile_only_over_the_square() {
    let device = device();
    let lab = scene([0.0, 5.0, 8.0]);
    let (vertices, indices) = quad(
        [[0.0, 2.0, 0.0], [1.0, 2.0, 0.0], [1.0, 2.0, 1.0], [0.0, 2.0, 1.0]],
        [0.0, 1.0, 0.0],
    );
    let material = LabMaterial::default();
    let draw = |renderer: &mut FrameRenderer, x: f32| {
        let mut scene = lab.as_frame_scene_with_quality(FrameQuality::LAB);
        let template = scene.meshes[0];
        let mut model = regia_core::glam::Mat4::IDENTITY.to_cols_array_2d();
        model[3][0] = x;
        scene.meshes.push(FrameMesh {
            mesh_key: 0x7700 << 3,
            object_key: 0x7700,
            material_key: 0x7701,
            material: &material,
            vertices: &vertices,
            indices: &indices,
            model,
            ..template
        });
        if let Err(err) = renderer.render_offscreen(&device, &scene) {
            panic!("frame: {err}");
        }
    };
    let mut renderer = renderer();
    draw(&mut renderer, 2.0);
    draw(&mut renderer, 2.0);
    let held = renderer.precipitation_occlusion().expect("on").redraws;
    draw(&mut renderer, 3.0);
    assert_eq!(
        renderer.precipitation_occlusion().expect("on").redraws,
        held + 1,
        "over the square"
    );
    draw(&mut renderer, 500.0);
    let away = renderer.precipitation_occlusion().expect("on").redraws;
    draw(&mut renderer, 510.0);
    assert_eq!(
        renderer.precipitation_occlusion().expect("on").redraws,
        away,
        "off the square"
    );
}

/// The local lights keep their tiles when the view turns on.
#[test]
fn the_tile_takes_no_room_from_the_lights() {
    let device = device();
    let mut lab = scene([0.0, 5.0, 8.0]);
    lab.local_lights
        .push(regia_render::LabLocalLight::Point(regia_render::LabPointLight {
            position: [0.0, 2.0, 0.0],
            color: [1.0; 3],
            intensity: 10_000.0,
            radius: 6.0,
            cast_shadows: true,
            source_radius: None,
        }));
    let plain = {
        let mut renderer = FrameRenderer::new();
        renderer.set_surface_weather(WET);
        match renderer.render_offscreen(&device, &lab.as_frame_scene_with_quality(FrameQuality::LAB)) {
            Ok(frame) => frame.pixels,
            Err(err) => panic!("frame: {err}"),
        }
    };
    let mut renderer = renderer();
    frame(&mut renderer, &device, &lab, FrameQuality::LAB);
    let shaded = match renderer.render_offscreen(&device, &lab.as_frame_scene_with_quality(FrameQuality::LAB))
    {
        Ok(frame) => frame.pixels,
        Err(err) => panic!("frame: {err}"),
    };
    let info = renderer.precipitation_occlusion().expect("on");
    assert_eq!(
        info.redraws, 1,
        "the light's redraw every frame does not touch the tile"
    );
    // The roofed floor differs (it is dry now); most of the frame does not.
    let same = plain
        .as_chunks::<4>()
        .0
        .iter()
        .zip(shaded.as_chunks::<4>().0)
        .filter(|(a, b)| a.iter().zip(b.iter()).all(|(x, y)| x.abs_diff(*y) <= 2))
        .count();
    assert!(same * 2 > (W * H) as usize, "{same}");
}

/// § 8: the read-back map answers 0 under the roof and 1 beside it, and where the cover is.
#[test]
fn the_map_says_what_is_covered() {
    let device = device();
    let lab = scene([0.0, 5.0, 8.0]);
    let mut renderer = renderer();
    assert!(
        renderer.take_precip_occlusion_readback().is_none(),
        "nothing asked yet"
    );
    renderer.request_precip_occlusion_readback();
    assert!(
        renderer.take_precip_occlusion_readback().is_none(),
        "no frame yet"
    );
    frame(&mut renderer, &device, &lab, FrameQuality::LAB);
    let map = renderer.take_precip_occlusion_readback().expect("copied");
    assert_eq!(map.size, 256);
    let (under, cover) = map.exposure_at([-3.0, 0.0, 0.0]);
    assert_eq!(under, 0.0);
    let cover = cover.expect("the roof is above");
    assert!((cover - 3.0).abs() < 0.05, "{cover}");
    let (beside, cover) = map.exposure_at([3.0, 0.0, 0.0]);
    assert_eq!(beside, 1.0);
    assert_eq!(cover, None);
    let (far, cover) = map.exposure_at([300.0, 0.0, 0.0]);
    assert_eq!((far, cover), (1.0, None), "outside the map");
    assert!(renderer.take_precip_occlusion_readback().is_none(), "one shot");
}

/// Spec 99 § 5.6: a swaying plant's rest pose is a fine shelter, so its sway never redraws the tile.
#[test]
fn a_swaying_caster_does_not_redraw_the_tile() {
    use regia_render::{DynamicCasterShadows, FoliageSway};
    const SURFACE: &str = r"
fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {
    return regia::pbr::surface::Surface(vec3<f32>(0.2, 0.6, 0.15), 1.0, 0.0, 0.8, in.world_normal, 1.0, vec3<f32>(0.0), 0.0);
}
";
    let device = device();
    let lab = scene([0.0, 5.0, 8.0]);
    let sway = FoliageSway {
        stiffness: 0.08,
        height_m: 1.6,
        flutter_m: 0.0,
        gust_period_s: 4.0,
        max_wind_m_per_s: 15.0,
        ..FoliageSway::default()
    };
    let material = LabMaterial {
        surface_graph: Some(std::sync::Arc::new(sway.graph(SURFACE))),
        ..LabMaterial::default()
    };
    let (vertices, indices) = quad(
        [
            [0.0, 0.0, -0.6],
            [0.0, 0.0, 0.6],
            [0.0, 1.6, 0.6],
            [0.0, 1.6, -0.6],
        ],
        [-1.0, 0.0, 0.0],
    );
    let mut renderer = renderer();
    renderer.set_dynamic_caster_shadows(DynamicCasterShadows::On {
        cascades: 2,
        local_lights: true,
    });
    let mut draw = |time: f32, wind: [f32; 2]| {
        let mut frame_scene = lab.as_frame_scene_with_quality(paged());
        let template = frame_scene.meshes[0];
        frame_scene.meshes.push(FrameMesh {
            mesh_key: 0x7710 << 3,
            object_key: 0x7710,
            material_key: 0x7711,
            material: &material,
            vertices: &vertices,
            indices: &indices,
            ..template
        });
        renderer.set_foliage_wind(Some(wind));
        renderer.set_material_clock(time, time - 1.0 / 60.0);
        if let Err(err) = renderer.render_offscreen(&device, &frame_scene) {
            panic!("frame: {err}");
        }
        renderer.precipitation_occlusion().expect("on").redraws
    };
    let first = draw(1.0, [2.0, 0.0]);
    assert_eq!(first, 1);
    for k in 0..4 {
        assert_eq!(draw(1.0 + k as f32 * 0.1, [12.0, 0.0]), 1, "frame {k}");
    }
}
