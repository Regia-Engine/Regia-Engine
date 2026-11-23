// SPDX-License-Identifier: MPL-2.0
//! A grey mesh and grey terrain under the same light print the same (lane vfx-look round 4).
//!
//! DO27's road and pad meshes printed brighter than the ground they lie on even after the
//! ground printed its authored reflectance (ADR-060): with both forced to 0.18 grey on the
//! Switchbacks plate, the road printed 1.4–1.7× the ground. The sun-only probe in
//! `terrain_look.rs` agreed within 3 %; the difference was the sky light. Meshes take the
//! atmosphere's directional irradiance map (a slope toward a low sun sees the bright
//! circumsolar sky), the shared terrain took a uniform two-colour dome: on 25° slopes a
//! mesh got 1.72× (toward the sun) or 0.66× (away) the terrain's sky light. The terrain
//! now reads the same irradiance map and split-sum table.
//!
//! A 0.18 slab lies on 0.18 ground (flat, and tilted 25° toward and away from a 14° Mars
//! sun) on a planet frame (atmosphere, IBL on, the runtime's flat ambient); the slab's
//! pixels are compared with the same pixels rendered without it, in sun and with the sun
//! off (full shadow).

use std::sync::OnceLock;

use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene,
    OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{
    CdlodConfig, LayerDesc, LayerTextures, MaterialParams, Rgba8Image, TerrainLayers, TerrainMap,
    TerrainMaterial, TerrainRelief, TerrainSharedDraw,
};

const W: u32 = 192;
/// Height of the ground (the camera sits 14 m above it).
const BASE_Y: f32 = 0.0;
const H: u32 = 108;

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

fn srgb_to_linear(c: u8) -> f64 {
    let c = f64::from(c) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// One frame of the 0.18 ground, with or without the 0.18 slab; `sun_lux` 0 is full shadow.
fn frame(sun_lux: f32, exposure_ev: f32, tilt_deg: f32, with_mesh: bool) -> Vec<u8> {
    let world = 64.0;
    // Ground rising toward -X: its normal leans toward +X by `tilt_deg`.
    let slope = tilt_deg.to_radians().tan();
    let heights: Vec<f32> = (0..65 * 65)
        .map(|i| {
            let x = ((i % 65) as f32 / 64.0 - 0.5) * world;
            BASE_Y - x * slope
        })
        .collect();
    let map = TerrainMap::from_heights(65, 65, heights, world, 80.0, BASE_Y - 40.0).expect("plane");
    let stack = TerrainLayers {
        layers: vec![LayerDesc {
            name: "ground".into(),
            albedo: [1.0, 1.0, 1.0],
            roughness: 1.0,
            tiling: 1.0,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -100.0,
            altitude_max: 100.0,
            blend_sharpness: 0.15,
            triplanar: false,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    };
    let v = regia_terrain::linear_to_srgb8(0.18);
    let mut draw = TerrainSharedDraw::new(device(), map, &stack, CdlodConfig::default(), 3).expect("terrain");
    draw.set_material(
        &TerrainMaterial {
            layers: vec![Some(LayerTextures {
                albedo: Rgba8Image::solid(64, 64, [v, v, v, 255]),
                normal: Rgba8Image::neutral_normal(64),
                height: Rgba8Image::neutral_height(64),
                orm: Rgba8Image::solid(64, 64, [255, 255, 0, 255]),
                use_orm: false,
            })],
            macro_set: None,
            params: MaterialParams::default(),
        },
        &[],
    )
    .expect("material");
    draw.set_relief(TerrainRelief::off());
    let post = PostSettings {
        exposure_ev,
        ..PostSettings::golden_deterministic()
    };
    // A 0.18-grey slab, 0.1 m thick, lying on the ground in the right half of the frame.
    let (verts, idx) = regia_render::box_mesh([3.0, 0.05, 3.0]);
    // Rotate the slab onto the slope (about Z by -tilt) and seat it 3.5 m down-slope.
    let (sin, cos) = (-tilt_deg.to_radians()).sin_cos();
    let rot = |v: [f32; 3]| [v[0] * cos - v[1] * sin, v[0] * sin + v[1] * cos, v[2]];
    let normal = rot([0.0, 1.0, 0.0]);
    let along = rot([1.0, 0.0, 0.0]);
    let at = [
        along[0] * 3.5 + normal[0] * 0.06,
        along[1] * 3.5 + normal[1] * 0.06,
        0.0,
    ];
    let verts: Vec<_> = verts
        .iter()
        .map(|v| {
            let p = rot(v.position);
            regia_render::LabVertex {
                position: [p[0] + at[0], p[1] + at[1], p[2] + at[2]],
                normal: rot(v.normal),
                tangent: {
                    let t = rot([v.tangent[0], v.tangent[1], v.tangent[2]]);
                    [t[0], t[1], t[2], v.tangent[3]]
                },
                ..*v
            }
        })
        .collect();
    let eye = [normal[0] * 14.0, BASE_Y + normal[1] * 14.0, 0.3];
    let scene = LabScene {
        width: W,
        height: H,
        eye,
        target: [0.0, BASE_Y, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 2000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: Some(post.clone()),
        // A low sun (14°) from +X, as on DO27's Switchbacks plates.
        directional: DirectionalLightUniform::new([-0.97, -0.24, 0.0], [1.0, 0.72, 0.48], sun_lux),
        local_lights: Vec::new(),
        materials: vec![regia_render::LabMaterial {
            base_color: [0.18, 0.18, 0.18, 1.0],
            metallic: 0.0,
            roughness: 1.0,
            ..regia_render::LabMaterial::default()
        }],
        meshes: if with_mesh {
            vec![regia_render::LabMesh {
                vertices: verts,
                indices: idx,
                material: 0,
                ..regia_render::LabMesh::unskinned()
            }]
        } else {
            Vec::new()
        },
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Mars),
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    // Desktop High's GPU irradiance probes converge over their first updates (the first frame
    // has no bounce yet); the analytic terms give the same image every frame.
    for _ in 0..11 {
        renderer
            .render_offscreen(device(), &frame)
            .expect("settling frame");
    }
    let px = renderer.render_offscreen(device(), &frame).expect("frame").pixels;
    if let Some(dir) = regia_core::brand::env_var_os("MESH_TERRAIN_DUMP") {
        let path =
            std::path::Path::new(&dir).join(format!("sun{sun_lux}_tilt{tilt_deg}_mesh{with_mesh}.png"));
        if let Ok(file) = std::fs::File::create(&path) {
            let mut encoder = png::Encoder::new(file, W, H);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            if let Ok(mut writer) = encoder.write_header() {
                let _ = writer.write_image_data(&px);
            }
        }
    }
    px
}

/// Display-linear luma of a pixel.
fn luma(px: &[u8], i: usize) -> f64 {
    0.2126 * srgb_to_linear(px[i]) + 0.7152 * srgb_to_linear(px[i + 1]) + 0.0722 * srgb_to_linear(px[i + 2])
}

/// (ground, mesh): mean luma over the slab's top in the frame with the slab, and over the
/// same pixels without it (the 0.18 ground the slab lies on).
fn ground_and_mesh(sun_lux: f32, exposure_ev: f32, tilt_deg: f32) -> (f64, f64) {
    let with = frame(sun_lux, exposure_ev, tilt_deg, true);
    let without = frame(sun_lux, exposure_ev, tilt_deg, false);
    // The slab's pixels: where it changes the frame (its edge and shadow excluded by
    // taking the middle of the changed region's bounding box).
    let (mut x0, mut x1, mut y0, mut y1) = (W, 0, H, 0);
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            if (0..3).any(|c| with[i + c].abs_diff(without[i + c]) > 0) {
                x0 = x0.min(x);
                x1 = x1.max(x);
                y0 = y0.min(y);
                y1 = y1.max(y);
            }
        }
    }
    assert!(x1 > x0 + 8 && y1 > y0 + 8, "the slab is not in view");
    let (mx, my) = ((x1 - x0) / 4, (y1 - y0) / 4);
    let (mut g, mut m, mut n) = (0.0, 0.0, 0.0);
    for y in y0 + my..y1 - my {
        for x in x0 + mx..x1 - mx {
            let i = ((y * W + x) * 4) as usize;
            g += luma(&without, i);
            m += luma(&with, i);
            n += 1.0;
        }
    }
    (g / n, m / n)
}

#[test]
fn a_grey_mesh_and_grey_terrain_print_the_same_lit_and_shadowed() {
    let mut fails = Vec::new();
    for tilt in [0.0, 25.0, -25.0] {
        for (name, sun, ev) in [("lit", 45_000.0, 13.0), ("shadowed", 0.0, 10.5)] {
            let (ground, mesh) = ground_and_mesh(sun, ev, tilt);
            let ratio = mesh / ground;
            eprintln!("tilt {tilt:+.0}° {name}: ground {ground:.4} mesh {mesh:.4} mesh/ground {ratio:.3}");
            if (ratio - 1.0).abs() > 0.05 {
                fails.push(format!("tilt {tilt}° {name}: mesh/ground {ratio:.3}"));
            }
        }
    }
    assert!(fails.is_empty(), "a 0.18 mesh and 0.18 terrain differ: {fails:?}");
}

/// A 20 m-deep, 24 m-wide trench along Z on an airless world, sun 30° from the west so the
/// whole floor lies in the west wall's shadow and the east wall is sunlit; a 0.6-grey slab
/// on the floor (0.6-grey too, the planet ground albedo). `bounce` sets the terrain's
/// `TerrainRelief::bounce`, which drives both the terrain's and the meshes' bounce.
///
/// Rendered once per process for each input (ADR-2986): the frame is a pure function of its inputs
/// (a fresh renderer on the shared device each time), and the two trench tests and `slab_rect`
/// asked for the same six frames twelve times.
fn trench_frame(with_mesh: bool, bounce: f32, probes: bool) -> std::sync::Arc<Vec<u8>> {
    type Memo = std::sync::Mutex<
        std::collections::HashMap<(bool, u32, bool), std::sync::Arc<OnceLock<std::sync::Arc<Vec<u8>>>>>,
    >;
    static FRAMES: OnceLock<Memo> = OnceLock::new();
    let cell = {
        let mut frames = FRAMES
            .get_or_init(Memo::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::sync::Arc::clone(frames.entry((with_mesh, bounce.to_bits(), probes)).or_default())
    };
    std::sync::Arc::clone(
        cell.get_or_init(|| std::sync::Arc::new(render_trench_frame(with_mesh, bounce, probes))),
    )
}

/// One trench frame, rendered (see [`trench_frame`]).
fn render_trench_frame(with_mesh: bool, bounce: f32, probes: bool) -> Vec<u8> {
    let n = 257u32;
    let width = 128.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for _row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * width - width * 0.5;
            heights.push(-20.0 * (1.0 - ((x.abs() - 12.0) / 2.0).clamp(0.0, 1.0)));
        }
    }
    let map = TerrainMap::from_heights(n, n, heights, width, 24.0, -22.0).expect("trench");
    let stack = TerrainLayers {
        layers: vec![LayerDesc {
            name: "ice".into(),
            albedo: [1.0, 1.0, 1.0],
            roughness: 1.0,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -100.0,
            altitude_max: 100.0,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    };
    let v = regia_terrain::linear_to_srgb8(0.6);
    let mut draw = TerrainSharedDraw::new(device(), map, &stack, CdlodConfig::default(), 3).expect("terrain");
    draw.set_material(
        &TerrainMaterial {
            layers: vec![Some(LayerTextures {
                albedo: Rgba8Image::solid(64, 64, [v, v, v, 255]),
                normal: Rgba8Image::neutral_normal(64),
                height: Rgba8Image::neutral_height(64),
                orm: Rgba8Image::solid(64, 64, [255, 255, 0, 255]),
                use_orm: false,
            })],
            macro_set: None,
            params: MaterialParams::default(),
        },
        &[],
    )
    .expect("material");
    draw.set_relief(TerrainRelief {
        bounce,
        ..TerrainRelief::off()
    });
    let post = PostSettings {
        exposure_ev: 7.0,
        ..PostSettings::golden_deterministic()
    };
    let (verts, idx) = regia_render::box_mesh([2.0, 0.05, 2.0]);
    let verts = regia_render::translate_mesh(&verts, [-2.0, -19.94, 4.0]);
    let scene = LabScene {
        width: W,
        height: H,
        eye: [-2.0, -11.0, -3.0],
        target: [-2.0, -20.0, 4.0],
        fov_y: 0.9,
        near: 0.1,
        far: 2000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 7.0,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([0.866, -0.5, 0.0], [1.0, 1.0, 1.0], 4_400.0),
        local_lights: Vec::new(),
        materials: vec![regia_render::LabMaterial {
            base_color: [0.6, 0.6, 0.6, 1.0],
            metallic: 0.0,
            roughness: 1.0,
            ..regia_render::LabMaterial::default()
        }],
        meshes: if with_mesh {
            vec![regia_render::LabMesh {
                vertices: verts,
                indices: idx,
                material: 0,
                ..regia_render::LabMesh::unskinned()
            }]
        } else {
            Vec::new()
        },
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    // `probes: false` is the tier without irradiance probes (the `GroundBounce` fallback).
    renderer.set_ddgi_enabled(Some(probes));
    let mut planet = AtmosphereParameters::from_preset(AtmospherePreset::Airless);
    planet.ground_albedo = [0.6, 0.6, 0.6];
    renderer.set_outdoor(OutdoorParams {
        planet,
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let px = renderer.render_offscreen(device(), &frame).expect("frame").pixels;
    if let Some(dir) = regia_core::brand::env_var_os("MESH_TERRAIN_DUMP") {
        let path = std::path::Path::new(&dir).join(format!("trench_mesh{with_mesh}_bounce{bounce}.png"));
        if let Ok(file) = std::fs::File::create(&path) {
            let mut encoder = png::Encoder::new(file, W, H);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            if let Ok(mut writer) = encoder.write_header() {
                let _ = writer.write_image_data(&px);
            }
        }
    }
    px
}

/// The middle of the slab's footprint on screen: where adding the slab changes the frame
/// (bounce on, so both frames are lit), shrunk by a quarter on each side.
fn slab_rect() -> (u32, u32, u32, u32) {
    let with = trench_frame(true, 1.0, false);
    let without = trench_frame(false, 1.0, false);
    let (mut x0, mut x1, mut y0, mut y1) = (W, 0, H, 0);
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            if (0..3).any(|c| with[i + c].abs_diff(without[i + c]) > 1) {
                x0 = x0.min(x);
                x1 = x1.max(x);
                y0 = y0.min(y);
                y1 = y1.max(y);
            }
        }
    }
    assert!(x1 > x0 + 8 && y1 > y0 + 8, "the slab is not in view");
    let (mx, my) = ((x1 - x0) / 4, (y1 - y0) / 4);
    (x0 + mx, x1 - mx, y0 + my, y1 - my)
}

/// (floor, slab) display-linear luma over `rect`.
fn trench_floor_and_slab(bounce: f32, rect: (u32, u32, u32, u32), probes: bool) -> (f64, f64) {
    let with = trench_frame(true, bounce, probes);
    let without = trench_frame(false, bounce, probes);
    let (mut g, mut m, mut n) = (0.0, 0.0, 0.0);
    for y in rect.2..rect.3 {
        for x in rect.0..rect.1 {
            let i = ((y * W + x) * 4) as usize;
            g += luma(&without, i);
            m += luma(&with, i);
            n += 1.0;
        }
    }
    (g / n, m / n)
}

#[test]
fn a_slab_on_a_shadowed_trench_floor_gets_the_floors_wall_bounce() {
    // Round 7: meshes standing on the terrain receive the light the sunlit walls bounce onto
    // the ground there (`GroundBounce`), so a rock or a slab in a shadowed trench prints like
    // the floor under it instead of darker.
    let rect = slab_rect();
    let (floor, slab) = trench_floor_and_slab(1.0, rect, false);
    let (floor_off, slab_off) = trench_floor_and_slab(0.0, rect, false);
    eprintln!(
        "trench floor/slab luma: bounce on {floor:.4}/{slab:.4} (ratio {:.3}), off {floor_off:.4}/{slab_off:.4}",
        slab / floor
    );
    assert!(
        floor > floor_off * 3.0,
        "the floor gets no bounce: {floor_off:.4} → {floor:.4}"
    );
    assert!(
        (slab / floor - 1.0).abs() < 0.05,
        "the slab prints {:.3}× the shadowed floor under it",
        slab / floor
    );
}

#[test]
fn with_probes_a_slab_on_a_shadowed_trench_floor_prints_like_the_floor() {
    // The irradiance probes trace the sunlit walls themselves: the floor and the slab on it
    // take the same probe light, with or without the terrain's bounce estimate.
    let rect = slab_rect();
    let (floor, slab) = trench_floor_and_slab(0.0, rect, true);
    let (floor_dark, _) = trench_floor_and_slab(0.0, rect, false);
    eprintln!(
        "probes: trench floor/slab luma {floor:.4}/{slab:.4} (ratio {:.3}); no probes, no bounce {floor_dark:.4}",
        slab / floor
    );
    assert!(
        floor > floor_dark * 3.0,
        "the probes add no bounce: {floor_dark:.4} → {floor:.4}"
    );
    assert!(
        (slab / floor - 1.0).abs() < 0.15,
        "with probes the slab prints {:.3}× the shadowed floor under it",
        slab / floor
    );
}
