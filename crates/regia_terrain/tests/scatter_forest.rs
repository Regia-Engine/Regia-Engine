// SPDX-License-Identifier: MPL-2.0
//! A streamed forest through the scatter field (ADR-1415): trees as multi-material assets (opaque bark,
//! alpha-tested twig cards, a far billboard part), a shadow LOD, the ECS frame bridge, and repeatability.
//!
//! The CPU tests need no GPU. `a_small_forest_renders_repeatably_on_the_gpu_driven_path` renders on the
//! machine's headless device (lavapipe in CI) and compares pixel statistics and two runs; no goldens.

use std::sync::Arc;

use regia_render::{
    ALPHA_MASK, DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, LabMaterial, LabMesh,
    LabScene, LabTexture, LabVertex, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
    SHADOW_LOD_MESH_TAG, ShadowCast,
};
use regia_terrain::{
    ScatterField, ScatterLibrary, ScatterMeshAsset, ScatterMeshLod, ScatterMeshPart, ScatterSet,
    ScatterSource, ScatterView, TerrainMap,
};

const SET: &str = r#"
ScatterSet(
    version: 2,
    seed: 17,
    classes: [
        ScatterClass(
            name: "woods",
            meshes: [
                ScatterMeshRef(mesh: "tree", weight: 1.0, min_scale: 0.9, max_scale: 1.1, scale_bias: 1.0),
            ],
            cell_size: 8.0,
            density: 0.6,
            base_blend: 0.0,
            fade_start: 120.0,
            fade_end: 160.0,
            cast_shadows: true,
            shadow_max_lod: 3,
            shadow_distance: 0.0,
        ),
    ],
)
"#;

const BARK: [f32; 4] = [0.30, 0.20, 0.10, 1.0];
const CARDS: [f32; 4] = [0.10, 0.50, 0.10, 1.0];
const BILLBOARD: [f32; 4] = [0.10, 0.40, 0.20, 1.0];

/// Which part a drawn mesh belongs to, from its material colour (the class tint and the variant shade scale
/// every channel alike, so the ratios survive).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    Bark,
    Cards,
    Billboard,
}

fn part_of(material: &LabMaterial) -> Part {
    let [r, g, b, _] = material.base_color;
    if r > g {
        Part::Bark
    } else if g > 3.5 * b {
        Part::Cards
    } else {
        Part::Billboard
    }
}

fn quad(corners: [[f32; 3]; 4], normal: [f32; 3]) -> (Vec<LabVertex>, Vec<u32>) {
    let uvs = [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]];
    let vertices = corners
        .iter()
        .zip(uvs)
        .map(|(p, uv)| LabVertex::from_pos_normal_uv(*p, normal, uv))
        .collect();
    (vertices, vec![0, 1, 2, 0, 2, 3])
}

/// Vertical quads about the Y axis, `count` of them spread over half a turn, from `y0` to `y1`, `half` wide.
fn crossed(count: usize, half: f32, y0: f32, y1: f32) -> ScatterMeshLod {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for k in 0..count {
        let a = std::f32::consts::PI * k as f32 / count as f32;
        let (s, c) = a.sin_cos();
        let (dx, dz) = (c * half, s * half);
        let (v, i) = quad(
            [[-dx, y0, -dz], [dx, y0, dz], [dx, y1, dz], [-dx, y1, -dz]],
            [-s, 0.0, c],
        );
        let base = vertices.len() as u32;
        vertices.extend(v);
        indices.extend(i.iter().map(|i| i + base));
    }
    ScatterMeshLod {
        vertices,
        indices,
        screen_size: 0.0,
    }
}

fn empty() -> ScatterMeshLod {
    ScatterMeshLod {
        vertices: Vec::new(),
        indices: Vec::new(),
        screen_size: 0.0,
    }
}

/// A 16×16 mask: opaque inside a disc, transparent outside (the cut-out a twig card has).
fn disc_texture(rgb: [u8; 3]) -> LabTexture {
    let n = 16u32;
    let mut rgba = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let (dx, dy) = (x as f32 + 0.5 - 8.0, y as f32 + 0.5 - 8.0);
            let inside = dx * dx + dy * dy < 36.0;
            rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], if inside { 255 } else { 0 }]);
        }
    }
    LabTexture {
        width: n,
        height: n,
        rgba,
    }
}

fn masked(base_color: [f32; 4], rgb: [u8; 3]) -> LabMaterial {
    LabMaterial {
        base_color,
        roughness: 0.9,
        alpha_mode: ALPHA_MASK,
        alpha_cutoff: 0.5,
        double_sided: true,
        albedo: Some(disc_texture(rgb)),
        ..LabMaterial::default()
    }
}

/// A 10 m stand-in tree: bark (LOD 0–2: 3, 2, 1 trunk planes), twig cards (3, 2, 1 crossed cards) and a
/// billboard part at LOD 3 only. Shadows come from LOD 1.
fn tree() -> ScatterMeshAsset {
    let mut lods = vec![
        crossed(3, 0.25, 0.0, 6.0),
        crossed(2, 0.25, 0.0, 6.0),
        crossed(1, 0.25, 0.0, 6.0),
        empty(),
    ];
    for (lod, size) in lods.iter_mut().zip([0.5, 0.2, 0.08, 0.0]) {
        lod.screen_size = size;
    }
    ScatterMeshAsset {
        lods,
        material: LabMaterial {
            base_color: BARK,
            roughness: 1.0,
            ..LabMaterial::default()
        },
        parts: vec![
            ScatterMeshPart {
                lods: vec![
                    crossed(3, 3.0, 4.0, 10.0),
                    crossed(2, 3.0, 4.0, 10.0),
                    crossed(1, 3.0, 4.0, 10.0),
                    empty(),
                ],
                material: masked(CARDS, [60, 160, 50]),
            },
            ScatterMeshPart {
                lods: vec![empty(), empty(), empty(), crossed(2, 3.0, 0.0, 10.0)],
                material: masked(BILLBOARD, [50, 130, 60]),
            },
        ],
        shadow_lod: 1,
    }
}

fn library() -> ScatterLibrary {
    let mut library = ScatterLibrary::new();
    library.insert("tree".into(), tree());
    library
}

fn field() -> ScatterField {
    let map = TerrainMap::from_heights(65, 65, vec![0.0; 65 * 65], 256.0, 1.0, 0.0).expect("flat map");
    let set = ScatterSet::from_ron(SET, None).expect("set");
    ScatterField::new(&set, ScatterSource::new(Arc::new(map)), &library(), 0x7_2EE5).expect("field")
}

fn view(eye: [f32; 3], target: [f32; 3]) -> ScatterView {
    ScatterView::look_at(eye, target, 0.9, 16.0 / 9.0, 0.1, 2000.0)
}

fn is_shadow_only(mesh: &FrameMesh<'_>) -> bool {
    mesh.mesh_key >> 56 == SHADOW_LOD_MESH_TAG >> 56
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn parts_follow_the_instance_lod_and_the_billboard_takes_over_far_out() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut f = field();
    let eye = [0.0, 1.7, 100.0];
    f.update(eye);
    let mut out = Vec::new();
    let stats = f.collect(&view(eye, [0.0, 1.7, -100.0]), &mut out);
    assert!(stats.emitted > 50, "{} trees emitted", stats.emitted);
    assert!(
        stats.per_lod[0] > 0 && stats.per_lod[3] > 0,
        "near and far trees: {:?}",
        stats.per_lod
    );
    let drawn: Vec<&FrameMesh<'_>> = out.iter().filter(|m| !is_shadow_only(m)).collect();
    // Every drawn mesh holds triangles; bark and cards never draw as billboards and the billboard never draws
    // the near geometry (two crossed planes, 4 triangles).
    for mesh in &drawn {
        assert!(!mesh.indices.is_empty());
        match part_of(mesh.material) {
            Part::Billboard => assert_eq!(mesh.indices.len(), 12, "the billboard part draws only its LOD 3"),
            Part::Bark | Part::Cards => assert!(mesh.indices.len() <= 18),
        }
    }
    let count = |part| drawn.iter().filter(|m| part_of(m.material) == part).count();
    let (bark, cards, billboards) = (count(Part::Bark), count(Part::Cards), count(Part::Billboard));
    assert!(bark > 0 && billboards > 0, "bark {bark}, billboards {billboards}");
    assert_eq!(bark, cards, "bark and cards are drawn together");
    // Object keys stay unique per drawn part.
    let mut keys: Vec<u64> = drawn.iter().map(|m| m.object_key).collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), drawn.len(), "every drawn part has its own object key");
}

#[test]
fn the_shadow_lod_casts_from_a_shadow_only_draw_of_the_coarser_mesh() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut f = field();
    let eye = [0.0, 1.7, 100.0];
    f.update(eye);
    let mut out = Vec::new();
    f.collect(&view(eye, [0.0, 1.7, -100.0]), &mut out);
    let shadow_only: Vec<&FrameMesh<'_>> = out.iter().filter(|m| is_shadow_only(m)).collect();
    assert!(!shadow_only.is_empty(), "near trees cast from LOD 1");
    for mesh in &shadow_only {
        assert_ne!(mesh.shadow, ShadowCast::Off, "a shadow-only draw casts");
        assert_ne!(part_of(mesh.material), Part::Billboard);
        // LOD 1: two planes of bark, two cards (12 indices each).
        assert_eq!(mesh.indices.len(), 12);
    }
    // A drawn LOD 0 part (three planes, 18 indices) never casts itself: its shadow is the LOD 1 copy.
    for mesh in out.iter().filter(|m| !is_shadow_only(m) && m.indices.len() == 18) {
        assert_eq!(
            mesh.shadow,
            ShadowCast::Off,
            "the drawn LOD 0 leaves the shadow to LOD 1"
        );
    }
    // Far trees draw LOD 2 or the billboard and cast from what they draw.
    assert!(
        out.iter().any(|m| !is_shadow_only(m)
            && part_of(m.material) == Part::Billboard
            && m.shadow != ShadowCast::Off),
        "billboards cast shadows"
    );
}

#[test]
fn the_ecs_bridge_hands_over_the_same_draws_as_collect() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut a = field();
    let mut b = field();
    let eye = [0.0, 1.7, 100.0];
    let v = view(eye, [0.0, 1.7, -100.0]);
    a.update(eye);
    b.update(eye);
    let mut meshes = Vec::new();
    let stats_a = a.collect(&v, &mut meshes);
    let mut extracted = Vec::new();
    let stats_b = b.extract_meshes(&v, &mut extracted);
    assert_eq!(stats_a, stats_b);
    assert_eq!(meshes.len(), extracted.len());
    for (m, e) in meshes.iter().zip(&extracted) {
        assert_eq!(e.entity.to_bits() ^ e.object_key_xor, m.object_key);
        assert_eq!(e.model, m.model);
        assert_eq!(e.shadow, m.shadow);
        assert_eq!(e.lod_dither.to_bits(), m.lod_dither.to_bits());
        assert_eq!(e.mesh.indices, m.indices);
        assert_eq!(e.mesh.vertices.len(), m.vertices.len());
        assert_eq!(e.material.base_color, m.material.base_color);
        assert_eq!(e.lod_key == SHADOW_LOD_MESH_TAG, is_shadow_only(m));
        assert!(!e.gi_proxy && e.joints.is_empty() && e.draw_indices.is_none());
    }
    // The bridge shares the field's meshes frame to frame (the renderer's mesh cache sees the same `Arc`).
    b.update(eye);
    let mut again = Vec::new();
    b.extract_meshes(&v, &mut again);
    assert_eq!(again.len(), extracted.len());
    for (x, y) in extracted.iter().zip(&again) {
        assert!(Arc::ptr_eq(&x.mesh, &y.mesh));
        assert!(Arc::ptr_eq(&x.material, &y.material));
    }
}

/// One draw as the renderer sees it: object key, mesh key, a hash of the model matrix, the dither bits, the index
/// count and whether it casts.
type Draw = (u64, u64, u64, u32, u32, bool);

/// Every frame of a dolly: everything that decides what the renderer draws.
fn dolly_frames(frames: usize) -> Vec<Vec<Draw>> {
    let mut f = field();
    (0..frames)
        .map(|i| {
            let z = 100.0 - i as f32 * 1.5;
            let eye = [0.0, 1.7, z];
            f.update(eye);
            let mut out = Vec::new();
            f.collect(&view(eye, [0.0, 1.7, z - 200.0]), &mut out);
            out.iter()
                .map(|m| {
                    let model: u64 = m.model.iter().flatten().fold(0xcbf2_9ce4_8422_2325u64, |h, v| {
                        (h ^ u64::from(v.to_bits())).wrapping_mul(0x0100_0000_01b3)
                    });
                    (
                        m.object_key,
                        m.mesh_key,
                        model,
                        m.lod_dither.to_bits(),
                        m.indices.len() as u32,
                        m.shadow != ShadowCast::Off,
                    )
                })
                .collect()
        })
        .collect()
}

#[test]
fn two_runs_of_a_dolly_emit_the_same_draws_in_the_same_order() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Background streaming and the parallel collect are on (the defaults): neither may change the output.
    let a = dolly_frames(60);
    let b = dolly_frames(60);
    assert_eq!(a.len(), b.len());
    for (i, (x, y)) in a.iter().zip(&b).enumerate() {
        assert_eq!(x, y, "frame {i} differs");
    }
    // The dolly crosses LOD switches, so the cross-fade copies (negative and positive dither) are part of what
    // was compared.
    let fading = a.iter().flatten().filter(|d| f32::from_bits(d.3) != 0.0).count();
    assert!(fading > 0, "no cross-fades on the dolly");
}

#[test]
fn malformed_parts_are_refused() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut short = tree();
    short.parts[0].lods.pop();
    assert!(
        short.validate("tree").is_err(),
        "a part with fewer LODs than the mesh"
    );
    let mut hollow = tree();
    hollow.parts[1].lods = vec![empty(), empty(), empty(), empty()];
    assert!(hollow.validate("tree").is_err(), "a part with no triangles");
    let mut no_lod0 = tree();
    no_lod0.lods[0] = empty();
    assert!(
        no_lod0.validate("tree").is_err(),
        "the mesh's LOD 0 must hold triangles"
    );
    let mut bad_index = tree();
    bad_index.parts[0].lods[1].indices[0] = 9999;
    assert!(
        bad_index.validate("tree").is_err(),
        "a part's indices stay in range"
    );
    assert!(tree().validate("tree").is_ok(), "empty coarse levels are fine");
}

fn ground() -> LabMesh {
    let n = [0.0, 1.0, 0.0];
    LabMesh {
        vertices: [[-200.0, 200.0], [200.0, 200.0], [200.0, -200.0], [-200.0, -200.0]]
            .iter()
            .map(|&[x, z]| LabVertex::from_pos_normal_uv([x, 0.0, z], n, [0.0, 0.0]))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material: 0,
        ..LabMesh::unskinned()
    }
}

const W: u32 = 256;
const H: u32 = 144;

/// Renders a 12-frame dolly through the small forest on the GPU-driven path with the forest switches on
/// (order-fixed cull, hashed alpha under TAA, alpha-tested shadows); returns per-frame pixel hashes and the
/// share of green (canopy) pixels in the last frame.
fn render_forest(device: &RenderDevice) -> (Vec<u64>, f32) {
    let mut f = field();
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.0,
        bloom: false,
        taa: true,
        ..PostSettings::golden_deterministic()
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.render_scale_permille = 1000;
    quality.gpu_driven = true;
    quality.screen_space_gi = false;
    quality.ssr = false;
    quality.volumetric_fog = false;
    quality.volumetric_clouds = false;
    quality.gi_probes = false;
    let mut renderer = FrameRenderer::new();
    renderer.set_hiz_occlusion(false);
    renderer.set_ordered_gpu_cull(true);
    renderer.set_taa_hashed_alpha(true);
    renderer.set_alpha_tested_shadows(true);
    let mut hashes = Vec::new();
    let mut green = 0.0;
    for i in 0..12 {
        let z = 60.0 - i as f32 * 0.5;
        let eye = [0.0, 4.0, z];
        let target = [0.0, 5.0, z - 60.0];
        f.update(eye);
        let lab = LabScene {
            width: W,
            height: H,
            eye,
            target,
            fov_y: 0.9,
            near: 0.1,
            far: 2000.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: true,
            exposure_ev: 14.0,
            ambient_lux: 12_000.0,
            double_sided: false,
            post: None,
            directional: DirectionalLightUniform::new([0.4, -0.7, -0.5], [1.0, 0.95, 0.9], 90_000.0),
            local_lights: Vec::new(),
            materials: vec![LabMaterial {
                base_color: [0.35, 0.3, 0.25, 1.0],
                roughness: 1.0,
                ..LabMaterial::default()
            }],
            meshes: vec![ground()],
        };
        let mut scene = lab.as_frame_scene_with_quality(quality);
        scene.post = Some(&post);
        let v = ScatterView::look_at(eye, target, 0.9, W as f32 / H as f32, 0.1, 2000.0);
        let mut trees: Vec<FrameMesh<'_>> = Vec::new();
        f.collect(&v, &mut trees);
        scene.meshes.extend(trees);
        let image = renderer
            .render_offscreen(device, &scene)
            .unwrap_or_else(|err| panic!("frame {i}: {err}"));
        hashes.push(image.pixels.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
        }));
        let pixels = image.pixels.as_chunks::<4>().0;
        green = pixels
            .iter()
            .filter(|p| p[1] > p[0] + 8 && p[1] > p[2] + 8)
            .count() as f32
            / pixels.len() as f32;
    }
    (hashes, green)
}

#[test]
fn a_small_forest_renders_repeatably_on_the_gpu_driven_path() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no headless GPU (lavapipe?): {err}"));
    let (a, green) = render_forest(&device);
    let (b, _) = render_forest(&device);
    eprintln!(
        "small forest: canopy pixels {:.1} %, sequence {:016x}",
        green * 100.0,
        a.iter().fold(0, |h, v| h ^ v)
    );
    assert!(
        green > 0.05,
        "the canopy covers {:.1} % of the frame",
        green * 100.0
    );
    assert_eq!(a, b, "two renders of the same dolly differ");
}

/// What the renderer sees of a collect: per draw, the object key, mesh key, index count, and whether it casts.
fn casters(f: &ScatterField, eye: [f32; 3]) -> Vec<(u64, u64, usize, bool, bool)> {
    let mut out = Vec::new();
    f.collect(&view(eye, [0.0, 1.7, -100.0]), &mut out);
    out.iter()
        .map(|m| {
            (
                m.object_key,
                m.mesh_key,
                m.indices.len(),
                m.shadow != ShadowCast::Off,
                is_shadow_only(m),
            )
        })
        .collect()
}

#[test]
fn a_field_wide_shadow_lod_hint_caps_the_scatter_shadow_lod() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut f = field();
    let eye = [0.0, 1.7, 100.0];
    f.update(eye);
    let base = casters(&f, eye);
    // Without a hint the near trees cast from the asset's LOD 1 (12 indices per bark/card part).
    assert!(base.iter().any(|d| d.4 && d.2 == 12), "LOD 1 shadow-only draws");
    // A neutral hint changes nothing.
    f.set_shadow_lod_hint(Some(regia_render::ShadowLodHint::default()));
    assert_eq!(f.shadow_lod_hint(), None, "a neutral hint is stored as none");
    assert_eq!(casters(&f, eye), base);
    // Capped at LOD 0 no instance casts from a coarser mesh than LOD 0's (18 indices).
    f.set_shadow_lod_hint(Some(regia_render::ShadowLodHint::cap(0)));
    let capped = casters(&f, eye);
    assert_ne!(capped, base, "the hint moved the shadows");
    for d in capped.iter().filter(|d| d.4) {
        assert_eq!(d.2, 18, "shadow-only draws are LOD 0 geometry");
    }
    // The near trees draw LOD 0 and now cast it themselves.
    assert!(
        capped.iter().any(|d| !d.4 && d.2 == 18 && d.3),
        "LOD 0 casts itself"
    );
    // Clearing the hint restores the frame exactly.
    f.set_shadow_lod_hint(None);
    assert_eq!(casters(&f, eye), base);
}
