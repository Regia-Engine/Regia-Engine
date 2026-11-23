// SPDX-License-Identifier: MIT-0
//! A wood in gusting wind (`--foliage-wind`, `docs/specs/88-foliage-wind.md` § 2.5, ADR-2304).
//!
//! A scatter field of procedural pines whose bark and crown materials sway
//! ([`FoliageSway::adopt`] on plain materials), one "imported" broadleaf tree (a textured plain
//! material, as a glTF import gives it) adopted the same way, and a painted patch of dead pines
//! painted with the brush's wind off ([`ScatterBrush::wind`]), so they stand still among the
//! swaying ones. The scene wind gusts on a fixed schedule ([`gust`]); the dynamic-caster shadow
//! class redraws the swaying trees' shadow pages every frame on Desktop High.
//!
//! ```text
//! cargo run -p terrain_lab -- --foliage-wind docs/evidence/foliage-wind
//! cargo run --release -p terrain_lab -- --foliage-wind /tmp/fw --bench --size 2560x1440
//! ```
//!
//! Writes `calm.png`, `gust.png` and `lull.png` and prints the class's report per shot. With
//! `--bench` it renders 120 frames with the class on (the tier default) and off and prints the
//! GPU shadow time of each (spec 88 § 8 budget: at most 0.5 ms more on a 5080 at 1440p).

use std::path::Path;
use std::sync::Arc;

use regia_core::glam::Vec2;
use regia_render::{
    DirectionalLightUniform, DynamicCasterShadows, FoliageSway, FrameMesh, FrameQuality, FrameRenderer,
    LabMaterial, LabMesh, LabScene, LabTexture, LabVertex, RenderDevice, RenderProfile, RenderProfilePreset,
};
use regia_terrain::{
    BrushShape, ScatterBrush, ScatterClass, ScatterField, ScatterLibrary, ScatterMeshAsset, ScatterMeshLod,
    ScatterMeshPart, ScatterMeshRef, ScatterSet, ScatterSource, ScatterView, Terrain, TerrainEdits,
    TerrainMap, TerrainScatterPaint, apply_scatter_stroke,
};

/// Side of the flat test ground, metres.
const WORLD_M: f32 = 256.0;
/// Ground height, metres.
const GROUND_Y: f32 = 10.0;
/// Prevailing wind direction (unit, world X and Z).
const WIND_DIR: [f32; 2] = [0.8, 0.6];

/// The scene wind at `t` seconds: a 3 m/s breeze along [`WIND_DIR`] with gusts up to 13 m/s.
/// Three incommensurate sines, each clamped at zero, so gusts arrive irregularly but the
/// schedule is a pure function of `t` (no random state): two runs see the same wind.
#[must_use]
pub fn gust(t: f32) -> [f32; 2] {
    let pulse = |period: f32, phase: f32| (std::f32::consts::TAU * (t / period + phase)).sin().max(0.0);
    let speed = 3.0 + 6.0 * pulse(7.0, 0.0) + 3.0 * pulse(3.1, 0.25) + 1.0 * pulse(1.3, 0.6);
    [WIND_DIR[0] * speed, WIND_DIR[1] * speed]
}

/// The shots and their clock times: calm at 4.1 s (the 3 m/s breeze alone, every gust pulse at
/// zero), the first gust at 1.75 s (9 m/s), a lull at 5.0 s (3.3 m/s, the gusts dying away).
const SHOTS: [(&str, f32); 3] = [("calm", 4.1), ("gust", 1.75), ("lull", 5.0)];

/// The pines' sway: a 9 m tree, stiff trunk, crown flutter, a 5 s gust period (divides 3600).
fn pine_sway() -> FoliageSway {
    FoliageSway {
        stiffness: 0.035,
        height_m: 9.0,
        flutter_m: 0.04,
        gust_period_s: 5.0,
        max_wind_m_per_s: 20.0,
        ..FoliageSway::default()
    }
}

/// A ring of `sides` vertices at height `y` and radius `r`, normals tilted up by `lean` (0 =
/// horizontal). Vertex colours stay white: they multiply the albedo, and red is also the sway's
/// flutter weight, so the bark's sway has no flutter instead ([`Wood::new`]).
fn ring(out: &mut Vec<LabVertex>, sides: u32, y: f32, r: f32, lean: f32) {
    for i in 0..sides {
        let a = std::f32::consts::TAU * i as f32 / sides as f32;
        let (s, c) = a.sin_cos();
        let n = Vec2::new(c, s) * (1.0 - lean * lean).max(0.0).sqrt();
        out.push(LabVertex::from_pos_normal_uv(
            [c * r, y, s * r],
            [n.x, lean, n.y],
            [i as f32 / sides as f32, y],
        ));
    }
}

/// Quads between consecutive rings of `sides` vertices starting at `first`.
fn stitch(indices: &mut Vec<u32>, first: u32, rings: u32, sides: u32) {
    for r in 0..rings - 1 {
        for i in 0..sides {
            let a = first + r * sides + i;
            let b = first + r * sides + (i + 1) % sides;
            let c = a + sides;
            let d = b + sides;
            indices.extend_from_slice(&[a, c, b, b, c, d]);
        }
    }
}

/// A pine: a trunk (part 0, bark) and a three-tier crown (part 1), each its own material.
fn pine(bark: LabMaterial, crown: LabMaterial) -> ScatterMeshAsset {
    let sides = 8;
    let mut trunk = Vec::new();
    ring(&mut trunk, sides, 0.0, 0.22, 0.0);
    ring(&mut trunk, sides, 3.0, 0.16, 0.0);
    ring(&mut trunk, sides, 8.8, 0.04, 0.0);
    let mut trunk_indices = Vec::new();
    stitch(&mut trunk_indices, 0, 3, sides);
    let mut leaves = Vec::new();
    let mut leaf_indices = Vec::new();
    for (y0, y1, r) in [(2.5, 6.0, 2.6), (4.5, 7.8, 1.9), (6.4, 9.0, 1.2)] {
        let first = leaves.len() as u32;
        ring(&mut leaves, sides, y0, r, 0.45);
        ring(&mut leaves, sides, 0.5 * (y0 + y1), 0.5 * r, 0.45);
        ring(&mut leaves, sides, y1, 0.02, 0.45);
        stitch(&mut leaf_indices, first, 3, sides);
    }
    let lod = |vertices: Vec<LabVertex>, indices: Vec<u32>| ScatterMeshLod {
        vertices,
        indices,
        screen_size: 0.0,
    };
    ScatterMeshAsset {
        lods: vec![lod(trunk, trunk_indices)],
        material: bark,
        parts: vec![ScatterMeshPart {
            lods: vec![lod(leaves, leaf_indices)],
            material: crown,
        }],
        shadow_lod: 0,
    }
}

/// A plain material with a procedural 8×8 RGBA texture (a stand-in for an imported map).
fn textured(color: [f32; 3], seed: u32) -> LabMaterial {
    let mut rgba = Vec::with_capacity(8 * 8 * 4);
    for i in 0..64u32 {
        let h = (i.wrapping_mul(2_654_435_761) ^ seed).rotate_left(7) % 64;
        let v = 170 + h as u8;
        rgba.extend_from_slice(&[v, v, v, 255]);
    }
    LabMaterial {
        base_color: [color[0], color[1], color[2], 1.0],
        roughness: 0.85,
        albedo: Some(LabTexture {
            width: 8,
            height: 8,
            rgba,
        }),
        double_sided: true,
        ..LabMaterial::default()
    }
}

/// Everything the shots need: the ground, the scatter field, the imported tree.
pub struct Wood {
    lab: LabScene,
    field: ScatterField,
    view: ScatterView,
    broadleaf: (Vec<LabVertex>, Vec<u32>, LabMaterial),
}

fn class(name: &str, density: f32) -> ScatterClass {
    ScatterClass {
        name: name.into(),
        meshes: vec![ScatterMeshRef {
            mesh: "pine".into(),
            weight: 1.0,
            min_scale: 0.8,
            max_scale: 1.3,
            scale_bias: 1.0,
        }],
        cell_size: 6.0,
        density,
        contact_ao: 0.0,
        fade_start: 300.0,
        fade_end: 360.0,
        ..ScatterClass::default()
    }
}

impl Wood {
    /// The wood seen from a clearing at `width`×`height` pixels. `swaying` off builds the same
    /// wood with plain materials (the before picture).
    ///
    /// # Panics
    ///
    /// Never on valid built-in inputs (the procedural scene); the sample panics with a message
    /// when the GPU or the scene setup fails.
    #[must_use]
    pub fn new(width: u32, height: u32, swaying: bool) -> Self {
        // The bark and the crown bend the same (same height, stiffness and gust), so the tree
        // stays in one piece; only the needles flutter.
        let crown_sway = pine_sway();
        let bark_sway = FoliageSway {
            flutter_m: 0.0,
            ..crown_sway
        };
        let adopt = |sway: FoliageSway, m: LabMaterial| {
            if swaying {
                sway.adopt(&m)
                    .unwrap_or_else(|e| panic!("plain materials adopt: {e}"))
            } else {
                m
            }
        };
        let bark = adopt(bark_sway, textured([0.32, 0.22, 0.15], 1));
        let crown = adopt(crown_sway, textured([0.10, 0.28, 0.09], 2));
        let mut library = ScatterLibrary::new();
        library.insert("pine".into(), pine(bark, crown));
        let set = ScatterSet {
            seed: 27,
            classes: vec![class("pines", 0.02), class("dead_pines", 0.0)],
            ..ScatterSet::default()
        };
        let n = 129u32;
        let map = Arc::new(
            TerrainMap::from_heights(n, n, vec![GROUND_Y; (n * n) as usize], WORLD_M, 1.0, 0.0)
                .unwrap_or_else(|e| panic!("flat map: {e}")),
        );
        let mut field = ScatterField::new(&set, ScatterSource::new(Arc::clone(&map)), &library, 0xF0_11)
            .unwrap_or_else(|e| panic!("scatter field: {e}"));
        field.set_background_streaming(false);
        // A patch of dead pines painted with the brush's wind off: they stand still.
        let heights = regia_asset::Heightfield {
            width: n,
            height: n,
            samples: vec![(GROUND_Y * 1000.0) as u16; (n * n) as usize],
        };
        let terrain = Terrain::new(
            &heights,
            WORLD_M,
            65.535,
            0.0,
            regia_terrain::CdlodConfig::default(),
            4,
            regia_terrain::TerrainLayers::default(),
        )
        .unwrap_or_else(|e| panic!("terrain: {e}"));
        let mut paint = TerrainScatterPaint::default();
        let mut edits = TerrainEdits::default();
        let brush = ScatterBrush {
            shape: BrushShape::Circle { radius: 9.0 },
            hardness: 1.0,
            density: 0.05,
            wind: false,
            ..ScatterBrush::for_class(&set.classes[1])
        };
        apply_scatter_stroke(
            &terrain,
            &mut paint,
            &mut edits,
            &set,
            &brush,
            &[Vec2::new(-14.0, -30.0)],
        )
        .unwrap_or_else(|e| panic!("calm stroke: {e}"));
        field.set_painted(&paint.0);

        let eye = [0.0, GROUND_Y + 6.0, 24.0];
        let target = [-4.0, GROUND_Y + 4.0, -20.0];
        let aspect = width as f32 / height as f32;
        let to_sun = [-0.55, 0.45, 0.70];
        let view = ScatterView::look_at(eye, target, 1.0, aspect, 0.1, 400.0).with_sun(to_sun);
        field.update(eye);
        field.settle_streaming(eye);

        let e = 0.5 * WORLD_M;
        let up = [0.0, 1.0, 0.0];
        let ground = LabMesh {
            vertices: vec![
                LabVertex::from_pos_normal_uv([-e, GROUND_Y, -e], up, [0.0, 0.0]),
                LabVertex::from_pos_normal_uv([e, GROUND_Y, -e], up, [64.0, 0.0]),
                LabVertex::from_pos_normal_uv([e, GROUND_Y, e], up, [64.0, 64.0]),
                LabVertex::from_pos_normal_uv([-e, GROUND_Y, e], up, [0.0, 64.0]),
            ],
            indices: vec![0, 2, 1, 0, 3, 2],
            ..LabMesh::unskinned()
        };
        let travel = [-to_sun[0], -to_sun[1], -to_sun[2]];
        let lab = LabScene {
            width,
            height,
            eye,
            target,
            fov_y: 1.0,
            near: 0.1,
            far: 400.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: false,
            exposure_ev: 14.0,
            ambient_lux: 8_000.0,
            double_sided: false,
            post: None,
            directional: DirectionalLightUniform::new(travel, [1.0, 0.96, 0.9], 90_000.0),
            local_lights: Vec::new(),
            materials: vec![textured([0.24, 0.30, 0.14], 3)],
            meshes: vec![ground],
        };
        // The imported broadleaf tree: one mesh, one textured plain material (what a glTF
        // import gives), adopted the same way: the pine's trunk under a crown 1.6 times wider.
        let tree = pine(LabMaterial::default(), LabMaterial::default());
        let mut vertices = tree.lods[0].vertices.clone();
        let mut indices = tree.lods[0].indices.clone();
        let base = vertices.len() as u32;
        indices.extend(tree.parts[0].lods[0].indices.iter().map(|i| i + base));
        vertices.extend(tree.parts[0].lods[0].vertices.iter().map(|v| {
            let mut v = *v;
            v.position[0] *= 1.6;
            v.position[2] *= 1.6;
            v
        }));
        let broadleaf_material = adopt(crown_sway, textured([0.16, 0.30, 0.08], 4));
        Self {
            lab,
            field,
            view,
            broadleaf: (vertices, indices, broadleaf_material),
        }
    }

    /// Renders one frame at clock `t` with the scene wind [`gust`]`(t)`.
    ///
    /// # Errors
    ///
    /// The renderer's error when the frame fails.
    pub fn render(
        &self,
        renderer: &mut FrameRenderer,
        device: &RenderDevice,
        quality: FrameQuality,
        t: f32,
    ) -> Result<Vec<u8>, regia_render::RenderError> {
        let mut scene = self.lab.as_frame_scene_with_quality(quality);
        let template = scene.meshes[0];
        let mut trees = Vec::new();
        self.field.collect(&self.view, &mut trees);
        scene.meshes.extend(trees);
        let (vertices, indices, material) = &self.broadleaf;
        let mut model =
            regia_core::glam::Mat4::from_translation([6.0, GROUND_Y, 0.0].into()).to_cols_array_2d();
        model[0][0] = 1.1;
        model[2][2] = 1.1;
        scene.meshes.push(FrameMesh {
            mesh_key: 0xB0AD_1EAF,
            object_key: 0xB0AD_1EAF,
            material_key: 0xB0AD_1EB0,
            material,
            vertices,
            indices,
            model,
            ..template
        });
        renderer.set_foliage_wind(Some(gust(t)));
        renderer.set_material_clock(t, t - 1.0 / 60.0);
        renderer
            .render_offscreen(device, &scene)
            .map(|frame| frame.pixels)
    }
}

/// Desktop High, the tier whose default refreshes swaying casters in cascades 0 and 1.
#[must_use]
pub fn desktop_high() -> FrameQuality {
    FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh))
}

fn size_arg(args: &[String]) -> (u32, u32) {
    args.iter()
        .position(|a| a == "--size")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .unwrap_or((960, 540))
}

/// `--foliage-wind <dir> [--bench] [--size WxH]`.
pub fn run(args: &[String]) {
    let dir = args
        .iter()
        .position(|a| a == "--foliage-wind")
        .and_then(|i| args.get(i + 1))
        .filter(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "docs/evidence/foliage-wind".into());
    let (w, h) = size_arg(args);
    let device = RenderDevice::headless().unwrap_or_else(|e| panic!("headless GPU device: {e}"));
    let wood = Wood::new(w, h, true);
    let quality = desktop_high();
    if args.iter().any(|a| a == "--bench") {
        bench(&wood, &device, quality);
        return;
    }
    if let Err(e) = std::fs::create_dir_all(&dir) {
        panic!("create {dir}: {e}");
    }
    let mut renderer = FrameRenderer::new();
    for (name, t) in SHOTS {
        // Two frames warm the caches, the third is the shot.
        for k in [0.1, 0.05, 0.0] {
            let pixels = wood
                .render(&mut renderer, &device, quality, t - k)
                .unwrap_or_else(|e| panic!("frame: {e}"));
            if k == 0.0 {
                let path = Path::new(&dir).join(format!("{name}.png"));
                super::write_png(&path.to_string_lossy(), w, h, &pixels);
                let report = renderer.dynamic_caster_report();
                let wind = gust(t);
                println!(
                    "{name}: t {t:.2} s, wind {:.1} m/s, animated casters {}, dynamic pages {}, policy {:?} -> {}",
                    (wind[0] * wind[0] + wind[1] * wind[1]).sqrt(),
                    report.animated_casters,
                    report.dynamic_pages,
                    report.policy,
                    path.display()
                );
            }
        }
    }
}

/// GPU shadow time with the class on (tier) and off over 120 frames of a running clock.
fn bench(wood: &Wood, device: &RenderDevice, quality: FrameQuality) {
    for (name, mode) in [
        ("tier (on)", DynamicCasterShadows::Tier),
        ("off", DynamicCasterShadows::Off),
    ] {
        let mut renderer = FrameRenderer::new();
        renderer.set_dynamic_caster_shadows(mode);
        renderer.set_gpu_timing(device, true);
        for warm in 0..10 {
            let _ = wood.render(&mut renderer, device, quality, warm as f32 / 60.0);
        }
        let frames = 120u32;
        let (mut shadow_ms, mut frame_ms, mut pages, mut animated) = (0.0f64, 0.0f64, 0u64, 0u32);
        for frame in 0..frames {
            if let Err(e) = wood.render(&mut renderer, device, quality, 0.2 + frame as f32 / 60.0) {
                panic!("frame: {e}");
            }
            let t = renderer.gpu_timings();
            shadow_ms += f64::from(t.shadows_ms);
            frame_ms += f64::from(t.frame_ms);
            let report = renderer.dynamic_caster_report();
            pages += u64::from(report.dynamic_pages);
            animated = report.animated_casters;
        }
        let n = f64::from(frames);
        println!(
            "dynamic casters {name}: GPU shadows {:.3} ms, GPU frame {:.3} ms, {:.1} dynamic pages per frame, {animated} animated casters",
            shadow_ms / n,
            frame_ms / n,
            pages as f64 / n,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gusts_are_deterministic_bounded_and_along_the_prevailing_wind() {
        let mut fastest = 0.0f32;
        let mut slowest = f32::MAX;
        for i in 0..2000 {
            let t = i as f32 * 0.05;
            let w = gust(t);
            assert_eq!(w.map(f32::to_bits), gust(t).map(f32::to_bits));
            let speed = (w[0] * w[0] + w[1] * w[1]).sqrt();
            // Along the prevailing direction.
            assert!((w[0] * WIND_DIR[1] - w[1] * WIND_DIR[0]).abs() < 1e-4);
            fastest = fastest.max(speed);
            slowest = slowest.min(speed);
        }
        assert!(
            slowest >= 3.0 - 1e-4 && fastest <= 13.0 + 1e-4,
            "{slowest}..{fastest}"
        );
        assert!(fastest > 9.0, "gusts arrive: {fastest}");
    }

    #[test]
    fn the_shots_are_calm_gust_and_lull() {
        let speed = |t: f32| {
            let w = gust(t);
            (w[0] * w[0] + w[1] * w[1]).sqrt()
        };
        let [calm, gust_peak, lull] = SHOTS.map(|(_, t)| speed(t));
        assert!((calm - 3.0).abs() < 1e-3, "calm is the breeze alone: {calm}");
        assert!(gust_peak > 8.5, "{gust_peak}");
        assert!(lull > calm && lull < 4.0, "{lull}");
    }

    #[test]
    fn the_wood_sways_and_the_painted_patch_stands_still() {
        let wood = Wood::new(64, 40, true);
        let mut trees = Vec::new();
        wood.field.collect(&wood.view, &mut trees);
        let swaying = |m: &FrameMesh<'_>| FoliageSway::of_material(m.material).is_some();
        assert!(trees.iter().any(swaying), "seeded pines sway");
        assert!(
            trees.iter().any(|m| !swaying(m)),
            "the painted dead pines stand still"
        );
        assert!(FoliageSway::of_material(&wood.broadleaf.2).is_some());
        let plain = Wood::new(64, 40, false);
        let mut before = Vec::new();
        plain.field.collect(&plain.view, &mut before);
        assert!(before.iter().all(|m| m.material.surface_graph.is_none()));
    }

    #[test]
    fn a_gust_moves_the_frame_and_the_class_counts_the_trees() {
        let device = match RenderDevice::headless() {
            Ok(d) => d,
            Err(e) => panic!("no headless GPU (lavapipe?): {e}"),
        };
        let wood = Wood::new(96, 54, true);
        let mut renderer = FrameRenderer::new();
        let calm = wood
            .render(&mut renderer, &device, desktop_high(), 0.05)
            .expect("calm");
        let report = renderer.dynamic_caster_report();
        assert!(report.animated_casters > 0, "{report:?}");
        assert_eq!(
            report.policy.cascade_mask, 0b11,
            "Desktop High refreshes two cascades"
        );
        let gusty = wood
            .render(&mut renderer, &device, desktop_high(), 1.75)
            .expect("gust");
        let moved = calm
            .as_chunks::<4>()
            .0
            .iter()
            .zip(gusty.as_chunks::<4>().0)
            .filter(|(a, b)| a.iter().zip(b.iter()).any(|(x, y)| x.abs_diff(*y) > 2))
            .count();
        assert!(moved > 10, "the gust bends the trees: {moved} pixels moved");
    }
}
