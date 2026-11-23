// SPDX-License-Identifier: MPL-2.0
//! Scatter rocks keep the planet's bounce light in shadow (lavapipe).
//!
//! Airless Europa: the sky is black, so a rock in a cast shadow is lit only by the bounce off
//! the sunlit snow around it — UE's shadowed rocks keep that bounce. This builds the shared
//! terrain plus two identical stand-in rocks, one in sun and one under a floating slab that
//! shades it, renders the frame through `FrameRenderer` on the airless preset, and requires
//! the shadowed rock to keep at least a quarter of the lit rock's luma.

use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer,
    LabMaterial, LabScene, OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
    ShadowCast, SharedPassDraw,
};
use regia_terrain::{CdlodConfig, RockStyle, TerrainLayers, TerrainMap, TerrainSharedDraw, procedural_rock};

const W: u32 = 320;
const H: u32 = 180;
const SUN_LUX: f32 = 100_000.0;

fn layers() -> TerrainLayers {
    TerrainLayers::from_ron(
        r#"TerrainLayers(version: 1, wetness_default: 0.0, layers: [
            LayerDesc(name: "snow", albedo: (0.62, 0.64, 0.68), roughness: 0.9, tiling: 2.0,
                slope_min: 0.0, slope_max: 1.6, altitude_min: -1000.0, altitude_max: 5000.0,
                blend_sharpness: 0.15, triplanar: false),
        ])"#,
    )
    .expect("layers")
}

fn luma(p: &[u8]) -> f32 {
    0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2])
}

struct Rig {
    device: RenderDevice,
    renderer: FrameRenderer,
    post: PostSettings,
}

impl Rig {
    fn new() -> Self {
        let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
        let n = 129;
        let map = TerrainMap::from_heights(n, n, vec![0.0; (n * n) as usize], 128.0, 1.0, 0.0).expect("map");
        let draw =
            TerrainSharedDraw::new(&device, map, &layers(), CdlodConfig::default(), 4).expect("terrain");
        let mut renderer = FrameRenderer::new();
        renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
        // As a DO27 level does: the planet's sun strength matches the scene's sun.
        let mut planet = AtmosphereParameters::from_preset(AtmospherePreset::Airless);
        planet.sun_illuminance = SUN_LUX;
        renderer.set_outdoor(OutdoorParams {
            planet,
            ..OutdoorParams::default()
        });
        let post = PostSettings {
            auto_exposure: false,
            exposure_ev: 14.5,
            bloom: false,
            taa: false,
            ..PostSettings::default()
        };
        Self {
            device,
            renderer,
            post,
        }
    }

    /// One frame with the given rocks (index into `[lit, shadowed]`) and the slab.
    fn frame(&mut self, rocks: &[bool; 2]) -> Vec<u8> {
        let rock = procedural_rock(5, RockStyle::rounded([0.46, 0.49, 0.53]));
        let lod = &rock.lods[0];
        let slab_material = LabMaterial {
            base_color: [0.4, 0.4, 0.42, 1.0],
            roughness: 0.8,
            ..LabMaterial::default()
        };
        let (slab_v, slab_i) = regia_render::box_mesh([1.9, 0.15, 1.9]);
        let place = |x: f32, z: f32| {
            let mut m = [[0.0f32; 4]; 4];
            m[0][0] = 1.5;
            m[1][1] = 1.5;
            m[2][2] = 1.5;
            m[3] = [x, -0.15, z, 1.0];
            m
        };
        let lab = LabScene {
            width: W,
            height: H,
            eye: [0.0, 2.5, 9.0],
            target: [0.0, 0.6, 0.0],
            fov_y: 0.8,
            near: 0.1,
            far: 500.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: true,
            exposure_ev: 14.5,
            ambient_lux: 0.0,
            double_sided: false,
            post: Some(self.post.clone()),
            // Sun from behind-left of the camera, 30° up: the rocks' camera faces are lit.
            directional: DirectionalLightUniform::new([0.35, -0.5, -0.79], [1.0, 0.98, 0.95], SUN_LUX),
            local_lights: Vec::new(),
            materials: Vec::new(),
            meshes: Vec::new(),
        };
        let mut quality =
            FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
        quality.taau = false;
        quality.render_scale_permille = 1000;
        quality.physical_atmosphere = true;
        let mut scene = lab.as_frame_scene_with_quality(quality);
        scene.post = Some(&self.post);
        for (i, (&on, x)) in rocks.iter().zip([-2.2f32, 2.2]).enumerate() {
            if on {
                scene.meshes.push(FrameMesh {
                    is_static: false,
                    mesh_key: 0x5ca7_0000_0000_0101,
                    material_key: 0x5ca7_0000_0000_0102,
                    object_key: 0x5ca7_0000_0000_0200 + i as u64,
                    vertices: &lod.vertices,
                    indices: &lod.indices,
                    material: &rock.material,
                    model: place(x, 0.0),
                    joints: &[],
                    receiver_layer: 0,
                    shadow: ShadowCast::Local {
                        reach: 4.0,
                        max_cascades: 2,
                        shadow_scale: 1.0,
                        shadow_lift: 0.0,
                        anchor: None,
                        lod_switch_keeps_pages: false,
                    },
                    uv1: &[],
                    lod_dither: 0.0,
                });
            }
        }
        // The slab floats 3 m over the right rock, offset toward the sun so its shadow covers it.
        let mut slab_model = [[0.0f32; 4]; 4];
        slab_model[0][0] = 1.0;
        slab_model[1][1] = 1.0;
        slab_model[2][2] = 1.0;
        slab_model[3] = [2.2 - 0.35 / 0.5 * 3.0, 3.0, 0.79 / 0.5 * 3.0, 1.0];
        scene.meshes.push(FrameMesh {
            is_static: false,
            mesh_key: 0x5ca7_0000_0000_0301,
            material_key: 0x5ca7_0000_0000_0302,
            object_key: 0x5ca7_0000_0000_0303,
            vertices: &slab_v,
            indices: &slab_i,
            material: &slab_material,
            model: slab_model,
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        });
        let mut pixels = Vec::new();
        for _ in 0..3 {
            pixels = self
                .renderer
                .render_offscreen(&self.device, &scene)
                .unwrap_or_else(|err| panic!("frame: {err}"))
                .pixels;
        }
        pixels
    }
}

fn mean_rock_luma(with: &[u8], without: &[u8]) -> (f32, usize) {
    let mut sum = 0.0;
    let mut n = 0;
    for i in 0..(W * H) as usize {
        let d: i32 = (0..3)
            .map(|k| (i32::from(with[i * 4 + k]) - i32::from(without[i * 4 + k])).abs())
            .sum();
        if d > 12 {
            sum += luma(&with[i * 4..i * 4 + 4]);
            n += 1;
        }
    }
    (sum / n.max(1) as f32, n)
}

fn write_evidence(name: &str, pixels: &[u8]) {
    if regia_core::brand::env_var_os("REGIA_WRITE_EVIDENCE").is_none() {
        return;
    }
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("scatter_ambient");
    let _ = std::fs::create_dir_all(&dir);
    let Ok(file) = std::fs::File::create(dir.join(name)) else {
        return;
    };
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    if let Ok(mut writer) = encoder.write_header() {
        let _ = writer.write_image_data(pixels);
    }
}

#[test]
fn shadowed_rock_keeps_the_bounce_light_on_airless_europa() {
    let mut rig = Rig::new();
    let none = rig.frame(&[false, false]);
    let lit = rig.frame(&[true, false]);
    let shaded = rig.frame(&[false, true]);
    write_evidence("rocks_none.png", &none);
    write_evidence("rock_lit.png", &lit);
    write_evidence("rock_shaded.png", &shaded);
    let both = rig.frame(&[true, true]);
    write_evidence("rocks_both.png", &both);
    let (lit_luma, lit_px) = mean_rock_luma(&lit, &none);
    let (shaded_luma, shaded_px) = mean_rock_luma(&shaded, &none);
    eprintln!("lit rock {lit_luma:.1} over {lit_px} px, shadowed rock {shaded_luma:.1} over {shaded_px} px");
    assert!(
        lit_px > 200 && shaded_px > 200,
        "rocks too small to judge ({lit_px}, {shaded_px})"
    );
    assert!(
        shaded_luma >= 0.25 * lit_luma,
        "a rock in shadow goes black: {shaded_luma:.1} vs lit {lit_luma:.1}"
    );
}
