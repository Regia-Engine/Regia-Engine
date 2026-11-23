// SPDX-License-Identifier: MPL-2.0
//! Lower-detail leaf shadow casters per scatter class (ADR-3146).
//!
//! Counted: triangles the far cascades take from a forest fall by a stated factor, and each tree's
//! shadow coverage stays within a stated bound. The field-wide hint this extends is measured beside it.
//! One frame test renders the shadow on the GPU (lavapipe, no goldens).

use std::sync::Arc;

use regia_render::{
    ALPHA_MASK, DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, LabMaterial, LabMesh,
    LabScene, LabTexture, LabVertex, PostSettings, RenderDevice, SHADOW_LOD_MESH_TAG, ShadowCast,
    ShadowLodHint,
};
use regia_terrain::{
    CasterPart, ScatterCasterHint, ScatterField, ScatterLibrary, ScatterMeshAsset, ScatterMeshLod,
    ScatterMeshPart, ScatterSet, ScatterSource, ScatterView, TerrainMap, caster_coverage,
};

const SET: &str = r#"
ScatterSet(
    version: 2,
    seed: 11,
    classes: [
        ScatterClass(
            name: "woods",
            meshes: [
                ScatterMeshRef(mesh: "tree", weight: 1.0, min_scale: 1.0, max_scale: 1.0, scale_bias: 1.0),
            ],
            cell_size: 16.0,
            density: 0.5,
            base_blend: 0.0,
            fade_start: 300.0,
            fade_end: 360.0,
            cast_shadows: true,
            shadow_max_lod: 3,
            shadow_distance: 0.0,
        ),
        ScatterClass(
            name: "other",
            meshes: [
                ScatterMeshRef(mesh: "tree", weight: 1.0, min_scale: 1.0, max_scale: 1.0, scale_bias: 1.0),
            ],
            cell_size: 16.0,
            density: 0.2,
            base_blend: 0.0,
            fade_start: 300.0,
            fade_end: 360.0,
            cast_shadows: true,
            shadow_max_lod: 3,
            shadow_distance: 0.0,
        ),
    ],
)
"#;

/// Far-cascade view splits of the counted test, metres: a caster past [`FAR_SPLIT`] lands in the far
/// cascades.
const FAR_SPLIT: f32 = 60.0;
/// One card in this many at distance.
const KEEP: u8 = 4;
/// What the far cascades' triangles must fall by with [`KEEP`]: the leaf cards fall 4x, the bark stays.
const FAR_TRIANGLE_FACTOR: f32 = 3.0;
/// How far a thinned tree's shadow coverage may leave the whole tree's.
const COVERAGE_BOUND: f32 = 0.08;

/// `count` separate seeded leaf cards over a crown of radius 5 m centred at 7 m.
fn cards(count: usize, side: f32, screen_size: f32) -> ScatterMeshLod {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let mut state = 0x5eed_u64 ^ count as u64;
    let mut next = || {
        state = state.wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(23) ^ 0x1234_5678;
        ((state >> 40) as f32 / 16_777_216.0) * 2.0 - 1.0
    };
    for _ in 0..count {
        let dir = [next(), next(), next()];
        let len = (dir[0] * dir[0] + dir[1] * dir[1] + dir[2] * dir[2])
            .sqrt()
            .max(1e-3);
        let radius = 5.0 * next().abs().cbrt();
        let centre = dir.map(|d| d / len * radius);
        let (a, b) = ([next(), next(), next()], [next(), next(), next()]);
        let norm = |v: [f32; 3]| {
            let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-3);
            v.map(|c| c / l * side * 0.5)
        };
        let (a, b) = (norm(a), norm(b));
        let base = vertices.len() as u32;
        for (sa, sb, uv) in [
            (-1.0, -1.0, [0.0, 1.0]),
            (1.0, -1.0, [1.0, 1.0]),
            (1.0, 1.0, [1.0, 0.0]),
            (-1.0, 1.0, [0.0, 0.0]),
        ] {
            let p = [0, 1, 2].map(|k| centre[k] + a[k] * sa + b[k] * sb);
            vertices.push(LabVertex::from_pos_normal_uv(
                [p[0], p[1] + 7.0, p[2]],
                [0.0, 1.0, 0.0],
                uv,
            ));
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    ScatterMeshLod {
        vertices,
        indices,
        screen_size,
    }
}

/// `count` crossed trunk planes.
fn bark(count: usize, screen_size: f32) -> ScatterMeshLod {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for k in 0..count {
        let a = std::f32::consts::PI * k as f32 / count as f32;
        let (s, c) = a.sin_cos();
        let (dx, dz) = (c * 0.25, s * 0.25);
        let base = vertices.len() as u32;
        for (p, uv) in [
            ([-dx, 0.0, -dz], [0.0, 1.0]),
            ([dx, 0.0, dz], [1.0, 1.0]),
            ([dx, 7.0, dz], [1.0, 0.0]),
            ([-dx, 7.0, -dz], [0.0, 0.0]),
        ] {
            vertices.push(LabVertex::from_pos_normal_uv(p, [-s, 0.0, c], uv));
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    ScatterMeshLod {
        vertices,
        indices,
        screen_size,
    }
}

fn empty() -> ScatterMeshLod {
    ScatterMeshLod {
        vertices: Vec::new(),
        indices: Vec::new(),
        screen_size: 0.0,
    }
}

/// A 16×16 cut-out: opaque inside a disc, transparent outside (a twig card).
fn disc_texture() -> LabTexture {
    let n = 16u32;
    let mut rgba = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let (dx, dy) = (x as f32 + 0.5 - 8.0, y as f32 + 0.5 - 8.0);
            let inside = dx * dx + dy * dy < 49.0;
            rgba.extend_from_slice(&[60, 160, 50, if inside { 255 } else { 0 }]);
        }
    }
    LabTexture {
        width: n,
        height: n,
        rgba,
    }
}

/// A tree whose leaf cards stay dense until the last LOD: 1200, 600 and 300 cards, then a two-plane
/// billboard part at LOD 3 only.
fn tree() -> ScatterMeshAsset {
    ScatterMeshAsset {
        lods: vec![bark(3, 0.5), bark(2, 0.2), bark(1, 0.08), empty()],
        material: LabMaterial {
            base_color: [0.30, 0.20, 0.10, 1.0],
            roughness: 1.0,
            ..LabMaterial::default()
        },
        parts: vec![
            ScatterMeshPart {
                lods: vec![
                    cards(1200, 0.6, 0.5),
                    cards(600, 0.85, 0.2),
                    cards(300, 1.2, 0.08),
                    empty(),
                ],
                material: LabMaterial {
                    base_color: [0.10, 0.50, 0.10, 1.0],
                    roughness: 0.9,
                    alpha_mode: ALPHA_MASK,
                    alpha_cutoff: 0.5,
                    double_sided: true,
                    albedo: Some(disc_texture()),
                    ..LabMaterial::default()
                },
            },
            ScatterMeshPart {
                lods: vec![empty(), empty(), empty(), bark(2, 0.0)],
                material: LabMaterial {
                    base_color: [0.10, 0.40, 0.20, 1.0],
                    alpha_mode: ALPHA_MASK,
                    alpha_cutoff: 0.5,
                    double_sided: true,
                    albedo: Some(disc_texture()),
                    ..LabMaterial::default()
                },
            },
        ],
        shadow_lod: 0,
    }
}

fn field() -> ScatterField {
    let map = TerrainMap::from_heights(129, 129, vec![0.0; 129 * 129], 1024.0, 1.0, 0.0).expect("flat map");
    let set = ScatterSet::from_ron(SET, None).expect("set");
    let mut library = ScatterLibrary::new();
    library.insert("tree".into(), tree());
    let mut f =
        ScatterField::new(&set, ScatterSource::new(Arc::new(map)), &library, 0x7_2EE5).expect("field");
    f.set_background_streaming(false);
    f
}

const EYE: [f32; 3] = [0.0, 12.0, 0.0];

fn view() -> ScatterView {
    ScatterView::look_at(EYE, [0.0, 8.0, -100.0], 0.9, 16.0 / 9.0, 0.1, 2000.0)
}

/// Caster triangles as the frame's cascades take them: `(near, far)`, a caster counted in the far
/// cascades when it is anchored past [`FAR_SPLIT`] from the eye.
fn caster_triangles(meshes: &[FrameMesh<'_>]) -> (u64, u64) {
    let (mut near, mut far) = (0, 0);
    for mesh in meshes.iter().filter(|m| m.shadow != ShadowCast::Off) {
        let anchor = match mesh.shadow {
            ShadowCast::Local { anchor: Some(a), .. } => a,
            _ => [mesh.model[3][0], mesh.model[3][1], mesh.model[3][2]],
        };
        let d = (0..3).map(|k| (anchor[k] - EYE[k]).powi(2)).sum::<f32>().sqrt();
        if d > FAR_SPLIT {
            far += mesh.indices.len() as u64 / 3;
        } else {
            near += mesh.indices.len() as u64 / 3;
        }
    }
    (near, far)
}

/// `(object key, mesh key, indices, shadow != Off, model, shadow only)` of every draw.
type DrawKey = (u64, u64, usize, bool, [[u32; 4]; 4], bool);

fn draws(f: &ScatterField) -> Vec<DrawKey> {
    let mut out = Vec::new();
    f.collect(&view(), &mut out);
    out.iter()
        .map(|m| {
            (
                m.object_key,
                m.mesh_key,
                m.indices.len(),
                m.shadow != ShadowCast::Off,
                m.model.map(|c| c.map(f32::to_bits)),
                m.mesh_key & SHADOW_LOD_MESH_TAG == SHADOW_LOD_MESH_TAG,
            )
        })
        .collect()
}

fn thinning() -> ScatterCasterHint {
    ScatterCasterHint {
        from_lod: 1,
        leaf_keep_one_in: KEEP,
        ..ScatterCasterHint::default()
    }
}

#[test]
fn far_cascades_lose_triangles_by_the_stated_factor_and_near_ones_none() {
    let mut f = field();
    f.settle_streaming(EYE);
    let (mut before, mut after) = (Vec::new(), Vec::new());
    let whole_stats = f.collect(&view(), &mut before);
    let (near_whole, far_whole) = caster_triangles(&before);
    drop(before);
    let mut hinted = field();
    hinted.settle_streaming(EYE);
    assert!(hinted.set_class_caster_hint("woods", Some(thinning())));
    assert!(hinted.set_class_caster_hint("other", Some(thinning())));
    let thin_stats = hinted.collect(&view(), &mut after);
    let (near_thin, far_thin) = caster_triangles(&after);
    let factor = far_whole as f32 / far_thin.max(1) as f32;
    eprintln!(
        "caster triangles: near {near_whole} -> {near_thin}, far {far_whole} -> {far_thin} ({factor:.2}x); \
         per LOD {:?}",
        whole_stats.per_lod
    );
    assert!(far_whole > 0, "the view holds far casters");
    assert!(near_whole > 0, "the view holds near casters");
    assert!(
        factor >= FAR_TRIANGLE_FACTOR,
        "far cascades: {far_whole} -> {far_thin} is {factor:.2}x, not {FAR_TRIANGLE_FACTOR}x"
    );
    assert!(
        thin_stats.caster_triangles < whole_stats.caster_triangles,
        "the frame stat follows"
    );
    assert_eq!(
        (whole_stats.caster_triangles, thin_stats.caster_triangles),
        (near_whole + far_whole, near_thin + far_thin),
        "the frame stat is the count"
    );
    assert_eq!(
        whole_stats.emitted, thin_stats.emitted,
        "the same instances are drawn"
    );
}

/// Shadow coverage of one drawn tree: every mesh with the given model that casts.
fn tree_coverage(meshes: &[FrameMesh<'_>], model: &[[f32; 4]; 4], to_sun: [f32; 3]) -> f32 {
    let parts: Vec<CasterPart<'_>> = meshes
        .iter()
        .filter(|m| m.shadow != ShadowCast::Off && m.model == *model)
        .map(|m| CasterPart {
            vertices: m.vertices,
            indices: m.indices,
            material: Some(m.material),
        })
        .collect();
    // Object space: a square just wider than the crown, at far-cascade texels of about 15 cm.
    caster_coverage(&parts, to_sun, [0.0, 7.0, 0.0], 7.0, 96)
}

#[test]
fn a_thinned_tree_keeps_its_shadow_coverage_within_the_stated_bound() {
    let mut f = field();
    f.settle_streaming(EYE);
    let mut hinted = field();
    hinted.settle_streaming(EYE);
    hinted.set_class_caster_hint("woods", Some(thinning()));
    hinted.set_class_caster_hint("other", Some(thinning()));
    let (mut whole, mut thin) = (Vec::new(), Vec::new());
    f.collect(&view(), &mut whole);
    hinted.collect(&view(), &mut thin);
    // Every far caster at LOD 1 and 2: the same models in both collects, the two mesh sets differ.
    let far_models: Vec<[[f32; 4]; 4]> = thin
        .iter()
        .filter(|m| m.mesh_key & SHADOW_LOD_MESH_TAG == SHADOW_LOD_MESH_TAG)
        .map(|m| m.model)
        .collect();
    assert!(!far_models.is_empty(), "the hint made shadow-only draws");
    let mut worst = 0.0f32;
    let mut measured = 0;
    let mut seen: Vec<[[f32; 4]; 4]> = Vec::new();
    for model in far_models.iter().filter(|m| m[3][2] < -10.0) {
        if seen.contains(model) || seen.len() >= 6 {
            continue;
        }
        seen.push(*model);
        for to_sun in [[0.0, 1.0, 0.0], [0.5, 0.7, 0.2], [0.9, 0.35, -0.2]] {
            let (a, b) = (
                tree_coverage(&whole, model, to_sun),
                tree_coverage(&thin, model, to_sun),
            );
            let off = (b - a).abs() / a.max(1e-6);
            eprintln!(
                "tree at z {:.0}, sun {to_sun:?}: coverage {a:.4} -> {b:.4} ({:.1} %)",
                model[3][2],
                off * 100.0
            );
            assert!(a > 0.05, "the whole tree must cast ({a})");
            worst = worst.max(off);
            measured += 1;
        }
    }
    eprintln!("worst of {measured}: {:.1} %", worst * 100.0);
    assert!(measured >= 3, "{measured} measures");
    assert!(
        worst < COVERAGE_BOUND,
        "a thinned tree's coverage moved {:.1} %, past {:.0} %",
        worst * 100.0,
        COVERAGE_BOUND * 100.0
    );
}

#[test]
fn the_hint_is_per_class_and_keeps_near_trees() {
    let mut f = field();
    f.settle_streaming(EYE);
    let whole = draws(&f);
    assert!(f.set_class_caster_hint("woods", Some(thinning())));
    assert!(!f.set_class_caster_hint("no such class", Some(thinning())));
    let thinned = draws(&f);
    assert_ne!(whole, thinned, "the hint changes the casters");
    // Only LOD 1 and coarser moved: a drawn LOD 0 caster casts itself as before, and every draw that
    // was there stays with its object key (the drawn copy just stops casting).
    let lod0_tris = 3 * 2 + 1200 * 2;
    let near = |list: &[DrawKey]| {
        list.iter()
            .filter(|d| d.3 && d.2 >= lod0_tris * 3)
            .map(|d| (d.0, d.1, d.2))
            .collect::<Vec<_>>()
    };
    assert_eq!(near(&whole), near(&thinned));
    assert_eq!(f.class_caster_hint("woods"), Some(thinning()));
    assert_eq!(
        f.class_caster_hint("other"),
        None,
        "the other class is not touched"
    );
}

#[test]
fn a_cleared_or_neutral_hint_is_the_frame_without_one() {
    let mut f = field();
    f.settle_streaming(EYE);
    let whole = draws(&f);
    let before = f.state_generation();
    assert!(f.set_class_caster_hint("woods", Some(ScatterCasterHint::default())));
    assert_eq!(f.class_caster_hint("woods"), None, "neutral reads as none");
    assert_eq!(f.state_generation(), before, "a neutral hint changes nothing");
    assert_eq!(draws(&f), whole);
    f.set_class_caster_hint("woods", Some(thinning()));
    assert_ne!(draws(&f), whole);
    f.set_class_caster_hint("woods", None);
    assert_eq!(draws(&f), whole, "cleared: the frame without a hint");
}

/// The chain's LOD clamp applies from `from_lod` on too: with a floor at the billboard, far trees cast
/// the two billboard planes, and a billboard is too few cards to thin further.
#[test]
fn a_class_lod_floor_casts_from_the_billboard_and_billboards_do_not_thin() {
    let mut f = field();
    f.settle_streaming(EYE);
    let (mut whole, mut floor) = (Vec::new(), Vec::new());
    let whole_stats = f.collect(&view(), &mut whole);
    let (_, far_whole) = caster_triangles(&whole);
    drop(whole);
    f.set_class_caster_hint(
        "woods",
        Some(ScatterCasterHint {
            from_lod: 1,
            min_lod: 3,
            leaf_keep_one_in: KEEP,
            ..ScatterCasterHint::default()
        }),
    );
    f.set_class_caster_hint(
        "other",
        Some(ScatterCasterHint {
            from_lod: 1,
            min_lod: 3,
            ..ScatterCasterHint::default()
        }),
    );
    let floor_stats = f.collect(&view(), &mut floor);
    let (_, far_floor) = caster_triangles(&floor);
    eprintln!("far casters with a billboard floor: {far_whole} -> {far_floor}");
    assert!(far_floor * 20 < far_whole, "{far_whole} -> {far_floor}");
    assert_eq!(whole_stats.emitted, floor_stats.emitted);
    for mesh in floor
        .iter()
        .filter(|m| m.mesh_key & SHADOW_LOD_MESH_TAG == SHADOW_LOD_MESH_TAG)
    {
        assert_eq!(
            mesh.indices.len(),
            2 * 6,
            "a shadow-only draw of the billboard: whole"
        );
    }
}

/// What the existing field-wide hint already gives, measured: it brings the far cascades down too, and
/// takes the near shadows with it. That is why this lane extends it per class and by drawn LOD.
#[test]
fn the_field_wide_hint_alone_coarsens_the_near_shadows_too() {
    let mut whole_field = field();
    whole_field.settle_streaming(EYE);
    let mut whole = Vec::new();
    whole_field.collect(&view(), &mut whole);
    let (near_whole, far_whole) = caster_triangles(&whole);
    let mut wide = field();
    wide.settle_streaming(EYE);
    wide.set_shadow_lod_hint(Some(ShadowLodHint::fixed(3)));
    let mut wide_draws = Vec::new();
    wide.collect(&view(), &mut wide_draws);
    let (near_wide, far_wide) = caster_triangles(&wide_draws);
    let mut class = field();
    class.settle_streaming(EYE);
    class.set_class_caster_hint("woods", Some(thinning()));
    class.set_class_caster_hint("other", Some(thinning()));
    let mut class_draws = Vec::new();
    class.collect(&view(), &mut class_draws);
    let (near_class, far_class) = caster_triangles(&class_draws);
    eprintln!(
        "near/far caster triangles: none {near_whole}/{far_whole}, field-wide fixed(3) {near_wide}/{far_wide}, \
         per class {near_class}/{far_class}"
    );
    assert!(
        far_wide < far_whole,
        "the field-wide hint does thin the far cascades"
    );
    assert!(
        near_wide * 4 < near_whole,
        "and it takes the near shadows with it: {near_whole} -> {near_wide}"
    );
    // Inside the near split some trees already draw LOD 1, and a hint from LOD 1 thins them by design;
    // trees drawn at LOD 0 keep their casters bit for bit (`the_hint_is_per_class_and_keeps_near_trees`).
    assert!(
        near_class * 3 > near_whole * 2,
        "the class hint from LOD 1 keeps more than two thirds of the near casters: {near_whole} -> {near_class}"
    );
    assert!(
        near_class > 10 * near_wide,
        "and far more than the field-wide hint: {near_class} vs {near_wide}"
    );
}

/// Serialises device creation and teardown across this binary's test threads (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

const W: u32 = 320;
const H: u32 = 180;

fn ground() -> LabMesh {
    let half = 500.0;
    let n = [0.0, 1.0, 0.0];
    LabMesh {
        vertices: [[-half, -half], [half, -half], [half, half], [-half, half]]
            .iter()
            .map(|&[x, z]| LabVertex::from_pos_normal_uv([x, 0.0, z], n, [0.0, 0.0]))
            .collect(),
        indices: vec![0, 2, 1, 0, 3, 2],
        material: 0,
        ..LabMesh::unskinned()
    }
}

/// A 20 m opaque box on the ground at `(x, z)`: the control caster of the GPU shadow test.
fn block(x: f32, z: f32) -> LabMesh {
    let (h, top) = (10.0, 20.0);
    let faces: [([f32; 3], [f32; 3], [f32; 3]); 5] = [
        ([0.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]),
        ([1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [-1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
    ];
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for (n, u, v) in faces {
        let base = vertices.len() as u32;
        for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let p = [
                x + h * (n[0] + su * u[0] + sv * v[0]),
                top * 0.5 + h * (n[1] + su * u[1] + sv * v[1]),
                z + h * (n[2] + su * u[2] + sv * v[2]),
            ];
            vertices.push(LabVertex::from_pos_normal_uv(p, n, [0.0, 0.0]));
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    LabMesh {
        vertices,
        indices,
        material: 1,
        ..LabMesh::unskinned()
    }
}

/// What casts in a GPU shadow frame.
#[derive(Clone, Copy, PartialEq)]
enum Casters {
    /// The trees, as the field emits them.
    Trees,
    /// Nothing: the trees draw without shadow and their shadow-only draws are dropped.
    Nothing,
    /// Only a control box ([`block`]); the trees draw without shadow.
    Block,
}

/// One frame over the forest: the camera high up and looking down at the ground far away, the sun
/// casting the trees' shadows onto it. The trees themselves draw too.
fn render(device: &RenderDevice, f: &mut ScatterField, casters: Casters) -> Vec<u8> {
    let eye = [0.0, 90.0, 0.0];
    let target = [0.0, 0.0, -160.0];
    f.settle_streaming(eye);
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.0,
        bloom: false,
        ..PostSettings::golden_deterministic()
    };
    // The lab baseline with four cascades, as `scatter_shadow_reach` renders its shadows (no paged sun
    // shadows, no GPU-driven culling).
    let quality = FrameQuality {
        csm_cascade_count: 4,
        csm_resolution: 2048,
        ..FrameQuality::LAB
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_hiz_occlusion(false);
    renderer.set_alpha_tested_shadows(true);
    let mut meshes = vec![ground()];
    if casters == Casters::Block {
        meshes.push(block(-20.0, -110.0));
    }
    let lab = LabScene {
        width: W,
        height: H,
        eye,
        target,
        fov_y: 0.9,
        near: 0.5,
        far: 2000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.0,
        // No ambient, as `scatter_shadow_reach`: at 15 000 lux and EV 14 the tone curve kept the
        // shadowed ground above 0.8 of the lit ground, so not even a 20 m box read as a shadow.
        ambient_lux: 0.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.5, -0.7, -0.3], [1.0, 1.0, 1.0], 90_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.7, 0.7, 0.7, 1.0],
                roughness: 1.0,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.8, 0.1, 0.1, 1.0],
                roughness: 1.0,
                ..LabMaterial::default()
            },
        ],
        meshes,
    };
    let mut scene = lab.as_frame_scene_with_quality(quality);
    scene.post = Some(&post);
    // The ground receives shadows and casts none, as in `scatter_shadow_reach`.
    scene.meshes[0].shadow = ShadowCast::Off;
    let view =
        ScatterView::look_at(eye, target, 0.9, W as f32 / H as f32, 0.5, 2000.0).with_sun([-0.5, 0.7, 0.3]);
    let mut trees: Vec<FrameMesh<'_>> = Vec::new();
    f.collect(&view, &mut trees);
    if casters != Casters::Trees {
        trees.retain(|m| m.mesh_key & SHADOW_LOD_MESH_TAG != SHADOW_LOD_MESH_TAG);
        for mesh in &mut trees {
            mesh.shadow = ShadowCast::Off;
        }
    }
    eprintln!(
        "{} tree draws, {} casting",
        trees.len(),
        trees.iter().filter(|m| m.shadow != ShadowCast::Off).count()
    );
    scene.meshes.extend(trees);
    // Two warm frames, then the read: the first frames build the shadow pipelines and caches, as in
    // `scatter_shadow_reach`.
    for _ in 0..2 {
        renderer
            .render_offscreen(device, &scene)
            .unwrap_or_else(|err| panic!("warm frame: {err}"));
    }
    renderer
        .render_offscreen(device, &scene)
        .unwrap_or_else(|err| panic!("frame: {err}"))
        .pixels
}

/// Share of the ground pixels (grey in both frames) that `cast` darkens below 0.8 of the frame
/// `uncast`, where nothing casts: the shadow the casters threw, as `scatter_shadow_reach` measures it.
fn shadow_share(cast: &[u8], uncast: &[u8]) -> f32 {
    let grey = |p: &[u8; 4]| p[0].abs_diff(p[1]) < 6 && p[1].abs_diff(p[2]) < 6;
    let (a, b) = (cast.as_chunks::<4>().0, uncast.as_chunks::<4>().0);
    let ground: Vec<(u8, u8)> = a
        .iter()
        .zip(b)
        .filter(|(p, q)| grey(p) && grey(q) && q[1] > 32)
        .map(|(p, q)| (p[1], q[1]))
        .collect();
    let dark = ground
        .iter()
        .filter(|&&(p, q)| f32::from(p) < 0.8 * f32::from(q))
        .count();
    eprintln!("ground {} px, darkened {dark}", ground.len());
    dark as f32 / ground.len().max(1) as f32
}

/// The thinned forest's shadow on the ground, measured on the GPU: the share of ground pixels in shadow
/// moves less than twice the per-tree bound, and a cleared hint gives the frame without one byte for
/// byte. Paged sun shadows are off (see `render`).
#[test]
fn the_thinned_forest_casts_the_same_shadow_on_the_gpu_and_off_keeps_the_bytes() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no headless GPU (lavapipe?): {err}"));
    let uncast = render(&device, &mut field(), Casters::Nothing);
    let control = shadow_share(&render(&device, &mut field(), Casters::Block), &uncast);
    eprintln!("control box: {control:.4} of the ground in its shadow");
    assert!(
        control > 0.002,
        "the control box must shadow the ground ({control})"
    );
    let whole = render(&device, &mut field(), Casters::Trees);
    let mut hinted = field();
    hinted.set_class_caster_hint("woods", Some(thinning()));
    hinted.set_class_caster_hint("other", Some(thinning()));
    let thin = render(&device, &mut hinted, Casters::Trees);
    let (a, b) = (shadow_share(&whole, &uncast), shadow_share(&thin, &uncast));
    let off = (b - a).abs() / a.max(1e-6);
    eprintln!("ground in shadow: {a:.4} -> {b:.4} ({:.1} %)", off * 100.0);
    assert!(a > 0.02, "the forest must shadow the ground ({a})");
    assert!(
        off < 2.0 * COVERAGE_BOUND,
        "the thinned forest's shadow moved {:.1} %: {a} -> {b}",
        off * 100.0
    );
    assert_ne!(whole, thin, "the casters did change");
    hinted.set_class_caster_hint("woods", None);
    hinted.set_class_caster_hint("other", None);
    assert_eq!(
        render(&device, &mut hinted, Casters::Trees),
        whole,
        "cleared, the frame is the frame without a hint"
    );
}
