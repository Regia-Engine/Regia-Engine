// SPDX-License-Identifier: MPL-2.0
//! ADR-2483: the streamed-level cook bakes textured HLOD proxies when the level asks for them, and
//! cooks exactly as before when it does not.

use super::*;
use crate::hlod_atlas::{BakeImage, atlas_ids};
use regia_test_utils::unique_temp_dir;

fn guid(n: u128) -> Guid {
    Guid::from_u128(n)
}

fn registry() -> TypeRegistry {
    let mut registry = TypeRegistry::default();
    register_cook_types(&mut registry);
    registry
}

const TRANSFORM: &str = "regia_scene::transform::Transform";

fn at(x: f32, y: f32, z: f32) -> String {
    format!("(translation: ({x}, {y}, {z}))")
}

fn entity(id: u128, parent: Option<u128>, components: &[(&str, String)]) -> String {
    let parent = parent.map_or(String::new(), |p| format!("parent: Some(\"{}\"),", guid(p)));
    let body: String = components.iter().map(|(k, v)| format!("\"{k}\": {v},")).collect();
    format!(
        "(guid: \"{}\", name: \"e{id}\", {parent} components: {{ {body} }}),",
        guid(id)
    )
}

fn lvl() -> LevelId {
    LevelId(guid(0xabc))
}

/// ADR-2483 pin fixture: three cells of colored `Primitive` cubes and `MeshSource` cubes, HLOD on
/// with the default settings (atlas off).
fn atlas_pin_level(extra: &str) -> SceneFile {
    const PRIMITIVE: &str = "regia_render::primitives::Primitive";
    let mut records = Vec::new();
    for i in 0..4u32 {
        let x = 10.0 + i as f32 * 5.0;
        records.push(entity(
            u128::from(100 + i),
            None,
            &[
                (TRANSFORM, at(x, 0.0, 10.0 + i as f32)),
                (
                    PRIMITIVE,
                    "(format_version: 1, shape: Cuboid(size: (2.0, 3.0, 2.0)), color: (1.0, 0.5, 0.0, 1.0), \
                     roughness: 0.5, metallic: 0.0)"
                        .to_owned(),
                ),
            ],
        ));
        records.push(entity(
            u128::from(200 + i),
            None,
            &[
                (TRANSFORM, at(110.0 + i as f32 * 5.0, 0.0, 10.0)),
                (
                    "regia_render::runtime::MeshSource",
                    "(mesh: \"builtin:cube\", material: \"assets/m.material.ron\")".to_owned(),
                ),
            ],
        ));
        records.push(entity(
            u128::from(300 + i),
            None,
            &[
                (TRANSFORM, at(210.0 + i as f32 * 5.0, 0.0, 10.0 + 2.0 * i as f32)),
                (
                    PRIMITIVE,
                    "(format_version: 1, shape: Sphere(radius: 1.5), color: (0.1, 0.2, 0.9, 1.0), \
                     roughness: 0.5, metallic: 0.0)"
                        .to_owned(),
                ),
            ],
        ));
    }
    let text = format!(
        "(format_version: 1, entities: [{}], resources: {{ \"regia_scene::streaming::LevelStreaming\": (enabled: \
         true, cell_size_m: 100.0, load_radius_m: 400.0, unload_radius_m: 500.0{extra}) }})",
        records.concat()
    );
    SceneFile::from_ron_str(&text).expect("scene")
}

/// Every cooked file of `dir` by name with its BLAKE3, sorted.
fn atlas_pin_hashes(dir: &std::path::Path) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = std::fs::read_dir(dir)
        .expect("dir")
        .filter_map(Result::ok)
        .map(|e| {
            let bytes = std::fs::read(e.path()).expect("file");
            (
                e.file_name().to_string_lossy().into_owned(),
                blake3::hash(&bytes).to_hex().to_string(),
            )
        })
        .collect();
    out.sort();
    out
}

/// The BLAKE3 of every file the pin fixture cooked before ADR-2483 (integ/v035, atlas off), by file
/// name; the world index is compared through its version 1 form.
const PINNED: &[(&str, &str)] = &[
    (
        "0615edeb-3a75-3a90-ca41-378ae98e58da.fckd",
        "08ba3e4cda3efc0befee5afeed9fd62d6c97fcbec8615434a6df83cbae02d243",
    ),
    (
        "22a2e673-aa38-de94-234b-1e8798e65d09.fckd",
        "ae61d72fdcefe2861031a9eb7d74f85589d6ce3b4d327b52967bd7af7b717f4c",
    ),
    (
        "35c82ffe-2bb4-efa9-e043-5e22435794c8.fckd",
        "c1b249bc8eb0f24c182bd9402f6a33c86c94f67de197cc1bfe8d6c6953555602",
    ),
    (
        "53a4eaf3-7b18-e0a4-7946-b069482aff9d.fckd",
        "7aa8fbf2f5b83c8b6022a77be60e4626d71f74e02718a16c84205930594f3c2c",
    ),
    (
        "a18ab5a5-379b-4bb0-3c44-627bfe4ff7a8.fckd",
        "e3a19404e0985cb37cfeb481b938408877f051aea33668c2c6d7c823a80b5b35",
    ),
    (
        "b88833a1-35ab-cc8f-d04b-64dcc9197a09.fckd",
        "fad7740dd4861b913624ef74a5f950f2502f25be319b2439039b687dd3d31307",
    ),
    (
        "c99b3d80-273b-0887-e548-a351374bba7a.fckd",
        "b75c84231246ae6cc10a6667a68aa528bb127edeb8060843958360fcbc0aac22",
    ),
    (
        "c9dd31f9-5845-2a8c-5748-1993a8c15a3c.fckd",
        "871003883f6f764b05135d56e13f087b59620e41c0a6b22707b5f16e0ed11ef4",
    ),
    (
        "f0556c2c-3934-f4b5-624f-ee2c7ee4d961.fckd",
        "59161514ff43f6d318188680ee5a40119d3dfec100b7133c37cf4b4a81d9ac1a",
    ),
];

/// The always-loaded set of the pin fixture on integ/v035, and with `LevelStreaming`'s two atlas
/// fields (at their defaults) in its reflected resource.
const ALWAYS_LOADED_BEFORE: &str = "e3a19404e0985cb37cfeb481b938408877f051aea33668c2c6d7c823a80b5b35";
const ALWAYS_LOADED_AFTER: &str = "b1755c5cbf9547e7359f33720109cd3a65c42d381b39ad96e6af2142dbd8bedd";

#[test]
fn a_level_with_the_atlas_off_cooks_byte_identical_to_before() {
    let dir = unique_temp_dir("stream_atlas_off");
    cook_streamed_level(
        lvl(),
        &atlas_pin_level(""),
        &registry(),
        &(),
        Some(&dir),
        "a.level.ron",
    )
    .expect("cook")
    .expect("streamed");
    let index_file = cooked_file_name(world_index_id(lvl()));
    let mut hashes = atlas_pin_hashes(&dir);
    for (name, hash) in &mut hashes {
        if *name == index_file {
            let bytes = std::fs::read(dir.join(&*name)).expect("index");
            let mut index = WorldIndex::from_bytes(&bytes, lvl()).expect("decode");
            assert_eq!(index.format_version, WORLD_INDEX_FORMAT_VERSION);
            // ADR-4993: the cell VRAM estimate counts built-in primitives' meshes, so the cuboids'
            // cell is no longer 0. That is the only change to this index: with the estimates put
            // back to the old cook's 0 (no asset costs here) it hashes to the pinned bytes.
            assert!(
                index.cells.iter().any(|c| c.vram_bytes > 0),
                "the cuboids' cell has a VRAM estimate"
            );
            for cell in &mut index.cells {
                cell.vram_bytes = 0;
            }
            assert!(
                index
                    .hlod
                    .as_ref()
                    .expect("hlod")
                    .node_atlases
                    .iter()
                    .all(Option::is_none)
            );
            *hash = blake3::hash(&index.to_v1_bytes().expect("v1"))
                .to_hex()
                .to_string();
        }
    }
    // The always-loaded set carries the level's `LevelStreaming` resource, reflected: it now holds
    // the two new fields at their defaults, and nothing else changed (every other file, the index's
    // content included, is the integ/v035 cook's).
    let always_file = cooked_file_name(always_loaded_id(lvl()));
    for (name, hash) in &mut hashes {
        if *name == always_file {
            assert_eq!(*hash, ALWAYS_LOADED_AFTER, "the always-loaded set changed");
            *hash = ALWAYS_LOADED_BEFORE.to_owned();
        }
    }
    let pinned: Vec<(String, String)> = PINNED
        .iter()
        .map(|(n, h)| ((*n).to_owned(), (*h).to_owned()))
        .collect();
    assert_eq!(
        hashes, pinned,
        "the cook with the atlas off changed (the index is {index_file})"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Assets that know one material: a red-textured one at `assets/red.material.ron` and a gradient
/// at `assets/ramp.material.ron`.
struct TexturedAssets;

fn ramp_image() -> Arc<BakeImage> {
    let n = 32u32;
    let mut rgba = Vec::new();
    for y in 0..n {
        for x in 0..n {
            rgba.extend_from_slice(&[(x * 255 / (n - 1)) as u8, (y * 255 / (n - 1)) as u8, 40, 255]);
        }
    }
    Arc::new(BakeImage::new(n, n, rgba).expect("image"))
}

impl StreamAssets for TexturedAssets {
    fn cost(&self, _id: AssetId) -> Option<AssetCost> {
        None
    }

    fn bake_material(&self, path: &str) -> Option<Arc<BakeMaterial>> {
        match path {
            "assets/red.material.ron" => Some(Arc::new(BakeMaterial {
                base_color_map: Some(Arc::new(BakeImage::new(2, 2, [230u8, 10, 10, 255].repeat(4))?)),
                roughness: 0.3,
                ..BakeMaterial::default()
            })),
            "assets/ramp.material.ron" => Some(Arc::new(BakeMaterial {
                base_color_map: Some(ramp_image()),
                metallic: 1.0,
                ..BakeMaterial::default()
            })),
            _ => None,
        }
    }

    fn platform(&self) -> &str {
        "linux"
    }
}

/// One cell of two textured boxes (a 4 m red one and a 6 m gradient one), atlas at `px`.
fn two_box_level(px: u32, max_px: u32) -> SceneFile {
    let boxes = [
        (1u128, [20.0, 2.0, 20.0], 4.0, "assets/red.material.ron"),
        (2u128, [40.0, 3.0, 30.0], 6.0, "assets/ramp.material.ron"),
    ];
    let records: Vec<String> = boxes
        .iter()
        .map(|(id, [x, y, z], size, material)| {
            entity(
                *id,
                None,
                &[
                    (
                        TRANSFORM,
                        format!("(translation: ({x}, {y}, {z}), scale: ({size}, {size}, {size}))"),
                    ),
                    (
                        "regia_render::runtime::MeshSource",
                        format!("(mesh: \"builtin:cube\", material: \"{material}\")"),
                    ),
                ],
            )
        })
        .collect();
    let text = format!(
        "(format_version: 1, entities: [{}], resources: {{ \"regia_scene::streaming::LevelStreaming\": (enabled: \
         true, cell_size_m: 100.0, load_radius_m: 400.0, unload_radius_m: 500.0, hlod_atlas_px: {px}, \
         hlod_atlas_max_px: {max_px}) }})",
        records.concat()
    );
    SceneFile::from_ron_str(&text).expect("scene")
}

fn srgb(v: u8) -> f32 {
    let v = f32::from(v) / 255.0;
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

#[test]
fn a_cell_of_two_textured_boxes_cooks_a_proxy_atlas_with_the_source_colors() {
    let dir = unique_temp_dir("stream_atlas_on");
    let output = cook_streamed_level(
        lvl(),
        &two_box_level(8, 1024),
        &registry(),
        &TexturedAssets,
        Some(&dir),
        "a.level.ron",
    )
    .expect("cook")
    .expect("streamed");
    assert!(
        output
            .report
            .warnings
            .iter()
            .all(|w| !w.code.starts_with("stream_hlod_atlas")),
        "{:?}",
        output.report.warnings
    );
    let read = |id: AssetId| std::fs::read(dir.join(cooked_file_name(id))).expect("cooked file");
    let index = WorldIndex::from_bytes(&read(world_index_id(lvl())), lvl()).expect("index");
    assert_eq!(index.settings.hlod_atlas_px, 8);
    let hlod = index.hlod.as_ref().expect("hlod");
    let tree = hlod.tree(&index.settings.grid().expect("grid")).expect("tree");
    let leaf = tree.leaf_for(CellCoord::new(0, 0)).expect("leaf") as usize;
    let proxy = hlod.node_proxies[leaf].expect("proxy");
    let atlas = hlod.atlas(leaf).expect("the leaf has an atlas");
    assert_eq!(atlas.material, atlas_ids(proxy)[3]);
    assert!(!atlas.capped);
    assert!(atlas.bake_error_m < 1e-3, "{}", atlas.bake_error_m);
    assert_eq!(atlas.miss_ratio, 0.0);
    // Every node with a proxy has an atlas, and the manifest names its four files.
    for (i, mesh) in hlod.node_proxies.iter().enumerate() {
        assert_eq!(mesh.is_some(), hlod.atlas(i).is_some(), "node {i}");
    }
    assert!(output.manifest.iter().any(|e| e.id == atlas.material));

    // The proxy mesh carries the atlas UVs; its positions are relative to the node's anchor.
    let mesh = regia_asset::load_mesh_bytes(&read(proxy)).expect("mesh").mesh;
    assert!(mesh.uvs.iter().any(|uv| uv[0] > 0.0 || uv[1] > 0.0));
    let space = LargeSpace::new(index.settings.origin.cell_size_m).expect("space");
    let anchor = hlod.node_anchors[leaf];
    let offset = Vec3::new(
        (anchor[0] as f64 * space.cell_size()) as f32,
        (anchor[1] as f64 * space.cell_size()) as f32,
        (anchor[2] as f64 * space.cell_size()) as f32,
    );
    // The base color the renderer would sample (BC7 decoded).
    let (_, texture) = regia_asset::load_texture_bytes(&read(atlas.textures[0])).expect("texture");
    let image = regia_render::CompressedTexture::from_cooked(&texture)
        .expect("ktx2")
        .decode_rgba8()
        .expect("decodes");
    assert_eq!([image.width, image.height], atlas.size);
    let sample = |p: Vec3| -> [f32; 3] {
        let local = p - offset;
        for tri in mesh.indices.as_chunks::<3>().0 {
            let v = tri.map(|i| Vec3::from_array(mesh.positions[i as usize]));
            let n = (v[1] - v[0]).cross(v[2] - v[0]);
            let area2 = n.length_squared();
            if area2 < 1e-12 || (local - v[0]).dot(n).abs() > 1e-3 * n.length() {
                continue;
            }
            let w = [
                (v[2] - v[1]).cross(local - v[1]).dot(n) / area2,
                (v[0] - v[2]).cross(local - v[2]).dot(n) / area2,
                (v[1] - v[0]).cross(local - v[0]).dot(n) / area2,
            ];
            if w.iter().any(|w| *w < -1e-4) {
                continue;
            }
            let uv = tri.map(|i| mesh.uvs[i as usize]);
            let u = uv[0][0] * w[0] + uv[1][0] * w[1] + uv[2][0] * w[2];
            let t = uv[0][1] * w[0] + uv[1][1] * w[1] + uv[2][1] * w[2];
            let x = ((u * image.width as f32) as u32).min(image.width - 1);
            let y = ((t * image.height as f32) as u32).min(image.height - 1);
            let at = ((y * image.width + x) * 4) as usize;
            return [
                srgb(image.rgba[at]),
                srgb(image.rgba[at + 1]),
                srgb(image.rgba[at + 2]),
            ];
        }
        panic!("{p} is not on the proxy");
    };
    // Known points on the red box's top and side faces.
    let red = [srgb(230), srgb(10), srgb(10)];
    for p in [
        Vec3::new(20.0, 4.0, 20.0),
        Vec3::new(21.0, 4.0, 19.2),
        Vec3::new(22.0, 2.5, 20.5),
        Vec3::new(19.0, 1.0, 22.0),
    ] {
        let got = sample(p);
        for c in 0..3 {
            assert!((got[c] - red[c]).abs() < 0.06, "red box at {p}: {got:?}");
        }
    }
    // The gradient box's top face: its color follows the face's UV, so it differs across the face
    // and every sample is a gradient color (blue channel fixed, red and green from the UV).
    let corners = [Vec3::new(38.0, 6.0, 28.0), Vec3::new(42.0, 6.0, 32.0)];
    let (a, b) = (sample(corners[0]), sample(corners[1]));
    assert!((a[0] - b[0]).abs() + (a[1] - b[1]).abs() > 0.2, "{a:?} {b:?}");
    for got in [a, b, sample(Vec3::new(40.0, 6.0, 30.0))] {
        assert!((got[2] - srgb(40)).abs() < 0.05, "{got:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn two_cooks_of_an_atlas_level_write_identical_bytes() {
    let (a, b) = (
        unique_temp_dir("stream_atlas_det_a"),
        unique_temp_dir("stream_atlas_det_b"),
    );
    for dir in [&a, &b] {
        cook_streamed_level(
            lvl(),
            &two_box_level(6, 512),
            &registry(),
            &TexturedAssets,
            Some(dir),
            "a.level.ron",
        )
        .expect("cook")
        .expect("streamed");
    }
    let (ha, hb) = (atlas_pin_hashes(&a), atlas_pin_hashes(&b));
    // The blob, the always-loaded set, the index, the proxy and its four atlas files.
    assert!(ha.len() >= 8, "{ha:?}");
    assert_eq!(ha, hb);
    for dir in [a, b] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn an_atlas_the_cap_truncates_cooks_with_a_warning() {
    let output = cook_streamed_level(
        lvl(),
        &two_box_level(256, 64),
        &registry(),
        &TexturedAssets,
        None,
        "a.level.ron",
    )
    .expect("cook")
    .expect("streamed");
    let capped: Vec<_> = output
        .report
        .warnings
        .iter()
        .filter(|w| w.code == "stream_hlod_atlas_capped")
        .collect();
    assert!(!capped.is_empty(), "{:?}", output.report.warnings);
    assert!(
        capped[0].message.contains("hlod_atlas_max_px"),
        "{}",
        capped[0].message
    );
    // A cook without a cache writes nothing and still reports.
    assert!(output.report.proxies > 0);
}

/// The residency budget sees a baked proxy's textures: its estimate is within a factor of three of
/// what the cook wrote, and the cap holds it.
#[test]
fn a_baked_proxy_counts_its_textures_in_the_residency_budget() {
    let cook = |px, max_px| {
        let dir = unique_temp_dir("hlod_atlas_budget");
        let output = cook_streamed_level(
            lvl(),
            &two_box_level(px, max_px),
            &registry(),
            &TexturedAssets,
            Some(&dir),
            "a.level.ron",
        )
        .expect("cook")
        .expect("streamed");
        let index = WorldIndex::from_bytes(
            &std::fs::read(dir.join(cooked_file_name(world_index_id(lvl())))).expect("index"),
            lvl(),
        )
        .expect("index");
        let hlod = index.hlod.expect("hlod");
        let leaf_bytes: u64 = hlod.proxies.iter().map(|p| p.bytes).sum();
        std::fs::remove_dir_all(&dir).ok();
        (leaf_bytes, output.report.proxy_bytes)
    };
    let (flat_leaves, _) = cook(0, 1024);
    let (baked_leaves, written) = cook(32, 1024);
    let (capped_leaves, _) = cook(32, 16);
    assert!(baked_leaves > flat_leaves, "{baked_leaves} over {flat_leaves}");
    assert!(
        baked_leaves * 3 > written && baked_leaves < written * 3,
        "the estimate {baked_leaves} follows the {written} bytes written"
    );
    assert!(
        capped_leaves < baked_leaves,
        "the cap holds the estimate: {capped_leaves} under {baked_leaves}"
    );
}

#[test]
fn bad_atlas_settings_are_refused_by_name() {
    let scene = two_box_level(8, 8);
    let err = cook_streamed_level(lvl(), &scene, &registry(), &TexturedAssets, None, "a.level.ron")
        .expect_err("cap below 16");
    assert!(err.to_string().contains("hlod_atlas_max_px"), "{err}");
}

// -- ADR-2713 ----------------------------------------------------------------------------------

/// The cooked world index of `dir`.
fn index_in(dir: &std::path::Path) -> WorldIndex {
    WorldIndex::from_bytes(
        &std::fs::read(dir.join(cooked_file_name(world_index_id(lvl())))).expect("index"),
        lvl(),
    )
    .expect("index")
}

/// After the bake, each leaf's bytes in the world index are its mesh's bytes plus the bytes of
/// the three texture files written for it, not the estimate the tree was built with.
#[test]
fn the_world_index_records_the_texture_bytes_written_for_each_leaf() {
    let cook = |px| {
        let dir = unique_temp_dir("hlod_atlas_exact");
        cook_streamed_level(
            lvl(),
            &two_box_level(px, 1024),
            &registry(),
            &TexturedAssets,
            Some(&dir),
            "a.level.ron",
        )
        .expect("cook")
        .expect("streamed");
        dir
    };
    let (flat_dir, baked_dir) = (cook(0), cook(32));
    let (flat, baked) = (index_in(&flat_dir), index_in(&baked_dir));
    let (flat_hlod, hlod) = (flat.hlod.expect("hlod"), baked.hlod.as_ref().expect("hlod"));
    let tree = hlod.tree(&baked.settings.grid().expect("grid")).expect("tree");
    assert!(!hlod.proxies.is_empty());
    for (leaf, mesh_only) in hlod.proxies.iter().zip(&flat_hlod.proxies) {
        assert_eq!(leaf.cell, mesh_only.cell);
        let node = tree.leaf_for(leaf.cell).expect("leaf") as usize;
        let atlas = hlod.atlas(node).expect("the leaf baked");
        let textures: u64 = atlas
            .textures
            .iter()
            .map(|t| {
                std::fs::metadata(baked_dir.join(cooked_file_name(*t)))
                    .expect("texture")
                    .len()
            })
            .sum();
        assert_eq!(leaf.bytes, mesh_only.bytes + textures, "cell {:?}", leaf.cell);
    }
    // The tree the runtime rebuilds from the exact bytes has the cook's shape.
    let flat_tree = flat_hlod
        .tree(&flat.settings.grid().expect("grid"))
        .expect("tree");
    assert_eq!(tree.nodes().len(), flat_tree.nodes().len());
    for (a, b) in tree.nodes().iter().zip(flat_tree.nodes()) {
        assert_eq!((a.cell, &a.children, a.level), (b.cell, &b.children, b.level));
    }
    std::fs::remove_dir_all(&flat_dir).ok();
    std::fs::remove_dir_all(&baked_dir).ok();
}

const TINT_GRAPH: &str = r#"(
    format_version: 2,
    params: [(name: "tint", value: Color((0.8, 0.2, 0.1)))],
    nodes: { "tint": Param("tint") },
    output: (base_color: Some("tint")),
)"#;

/// A cell of one box drawn with a material graph cooks a proxy atlas of the graph's color: the
/// cache reads the graph source from the project (ADR-2713). Without the project root the cache has no material
/// for it, as before.
#[test]
fn a_box_with_a_material_graph_cooks_its_proxy_in_the_graphs_color() {
    let root = unique_temp_dir("hlod_atlas_graph_cook");
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    std::fs::write(root.join("assets/tint.matgraph.ron"), TINT_GRAPH).expect("graph");
    let cache = root.join("cache");
    std::fs::create_dir_all(&cache).expect("cache");
    let without = CacheAssets::for_platform(&cache, "linux");
    assert!(without.bake_material("assets/tint.matgraph.ron").is_none());
    let assets = CacheAssets::for_platform(&cache, "linux").with_project_root(&root);
    let material = assets.bake_material("assets/tint.matgraph.ron").expect("graph");
    assert_eq!(material.base_color, [0.8, 0.2, 0.1, 1.0]);

    let text = format!(
        "(format_version: 1, entities: [{}], resources: {{ \"regia_scene::streaming::LevelStreaming\": (enabled: \
         true, cell_size_m: 100.0, load_radius_m: 400.0, unload_radius_m: 500.0, hlod_atlas_px: 8) }})",
        entity(
            1,
            None,
            &[
                (
                    TRANSFORM,
                    "(translation: (20.0, 2.0, 20.0), scale: (4.0, 4.0, 4.0))".to_owned()
                ),
                (
                    "regia_render::runtime::MeshSource",
                    "(mesh: \"builtin:cube\", material: \"assets/tint.matgraph.ron\")".to_owned(),
                ),
            ],
        )
    );
    let scene = SceneFile::from_ron_str(&text).expect("scene");
    let out = root.join("out");
    std::fs::create_dir_all(&out).expect("out");
    cook_streamed_level(lvl(), &scene, &registry(), &assets, Some(&out), "a.level.ron")
        .expect("cook")
        .expect("streamed");
    let index = index_in(&out);
    let hlod = index.hlod.as_ref().expect("hlod");
    let tree = hlod.tree(&index.settings.grid().expect("grid")).expect("tree");
    let leaf = tree.leaf_for(CellCoord::new(0, 0)).expect("leaf") as usize;
    let atlas = hlod.atlas(leaf).expect("atlas");
    let (_, texture) = regia_asset::load_texture_bytes(
        &std::fs::read(out.join(cooked_file_name(atlas.textures[0]))).expect("base"),
    )
    .expect("texture");
    let decoded = regia_render::CompressedTexture::from_cooked(&texture)
        .expect("ktx2")
        .decode_rgba8()
        .expect("decodes");
    let texels = decoded.rgba.as_chunks::<4>().0;
    let near_tint = texels
        .iter()
        .filter(|t| {
            (srgb(t[0]) - 0.8).abs() < 0.05
                && (srgb(t[1]) - 0.2).abs() < 0.05
                && (srgb(t[2]) - 0.1).abs() < 0.05
        })
        .count();
    assert!(
        near_tint * 10 >= texels.len() * 9,
        "{near_tint} of {} texels hold the graph's tint",
        texels.len()
    );
    std::fs::remove_dir_all(&root).ok();
}

/// Cook-time probe on a generated world (wall clock, printed, never asserted): the same level
/// cooked with the bake searching every near cell in turn and through the source index. Run with
/// `--ignored --nocapture`; `HLOD_PROBE_CELLS` sets the side in cells (default 16).
#[test]
#[ignore = "timing probe for the ADR-2713 report"]
fn zz_probe_cook_time_with_and_without_the_source_index() {
    let side: u32 = std::env::var("HLOD_PROBE_CELLS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(16);
    let px: u32 = std::env::var("HLOD_PROBE_PX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    // `HLOD_PROBE_PROPS=0`: ground tiles only, so parents merge them into a few large triangles.
    let props = std::env::var("HLOD_PROBE_PROPS").ok().is_none_or(|v| v != "0");
    let mut records = Vec::new();
    let mut id = 1u128;
    for j in 0..side {
        for i in 0..side {
            let (x, z) = (i as f32 * 64.0, j as f32 * 64.0);
            let boxes: &[(f32, f32, &str, f32)] = if props {
                &[
                    (16.0, 16.0, "assets/red.material.ron", 6.0),
                    (40.0, 24.0, "assets/ramp.material.ron", 4.0),
                    (24.0, 44.0, "assets/red.material.ron", 3.0),
                ]
            } else {
                &[]
            };
            for &(dx, dz, material, size) in boxes {
                records.push(entity(
                    id,
                    None,
                    &[
                        (
                            TRANSFORM,
                            format!(
                                "(translation: ({}, {}, {}), scale: ({size}, {size}, {size}))",
                                x + dx,
                                size * 0.5,
                                z + dz
                            ),
                        ),
                        (
                            "regia_render::runtime::MeshSource",
                            format!("(mesh: \"builtin:cube\", material: \"{material}\")"),
                        ),
                    ],
                ));
                id += 1;
            }
            records.push(entity(
                id,
                None,
                &[
                    (
                        TRANSFORM,
                        format!(
                            "(translation: ({}, 0.0, {}), scale: (64.0, 1.0, 64.0))",
                            x + 32.0,
                            z + 32.0
                        ),
                    ),
                    (
                        "regia_render::runtime::MeshSource",
                        "(mesh: \"builtin:plane\", material: \"assets/ramp.material.ron\")".to_owned(),
                    ),
                ],
            ));
            id += 1;
        }
    }
    let text = format!(
        "(format_version: 1, entities: [{}], resources: {{ \"regia_scene::streaming::LevelStreaming\": (enabled: \
         true, cell_size_m: 64.0, load_radius_m: 256.0, unload_radius_m: 320.0, hlod_atlas_px: {px}) }})",
        records.concat()
    );
    let scene = SceneFile::from_ron_str(&text).expect("scene");
    for linear in [true, false] {
        crate::hlod_atlas::TEST_LINEAR.set(linear);
        crate::hlod_atlas::TEST_COUNTS.set(None);
        crate::hlod_atlas::TEST_BAKE_NANOS.set(0);
        let dir = unique_temp_dir("hlod_atlas_probe");
        let start = std::time::Instant::now();
        let out = cook_streamed_level(
            lvl(),
            &scene,
            &registry(),
            &TexturedAssets,
            Some(&dir),
            "a.level.ron",
        )
        .expect("cook")
        .expect("streamed");
        let took = start.elapsed();
        let counts = crate::hlod_atlas::TEST_COUNTS.get().unwrap_or_default();
        println!(
            "PROBE props={props} cells={} px={px} search={} cook_s={:.2} bake_s={:.2} proxies={} queries={} cells_entered={} \
             index_nodes={} tree_nodes={} tree_triangles={} cutout_steps={} cells_entered/query={:.2} tests/query={:.2} \
             triangles/query={:.2}",
            side * side,
            if linear { "every-near-cell" } else { "index" },
            took.as_secs_f64(),
            crate::hlod_atlas::TEST_BAKE_NANOS.get() as f64 * 1e-9,
            out.report.proxies,
            counts.rays + counts.closest,
            counts.cells_entered,
            counts.index_nodes,
            counts.tree.nodes,
            counts.tree.triangles,
            counts.cutout_steps,
            counts.cells_entered as f64 / (counts.rays + counts.closest).max(1) as f64,
            counts.tests_per_query(),
            counts.triangles_per_query(),
        );
        std::fs::remove_dir_all(&dir).ok();
    }
    crate::hlod_atlas::TEST_LINEAR.set(false);
}

fn cooked_file(id: AssetId, type_tag: regia_asset::AssetTypeTag, payload: &[u8]) -> Vec<u8> {
    let header = regia_asset::CookedHeader {
        format_version: regia_asset::COOKED_FORMAT_VERSION,
        type_tag: type_tag.0,
        asset_id: id,
        importer_version: 1,
        dependencies: Vec::new(),
        payload_len: payload.len() as u64,
    };
    regia_asset::encode_cooked_file(&header, payload).expect("encode")
}

/// Two meshes that share one material decode each of its textures once, keep one `Arc` of the
/// image, and the proxy atlas stays that image's color.
#[test]
fn a_shared_material_decodes_each_texture_once() {
    let root = unique_temp_dir("stream_bake_once");
    let cache = root.join("cache");
    let out = root.join("out");
    std::fs::create_dir_all(&cache).expect("cache");
    std::fs::create_dir_all(&out).expect("out");

    let tex_id = AssetId(guid(0xC30));
    let mat_id = AssetId(guid(0xC31));
    let material_path = "assets/shared.material.ron";
    let rgba = [230u8, 10, 10, 255].repeat(8 * 8);
    let texture = crate::cook_texture_rgba8(
        8,
        8,
        &rgba,
        regia_asset::TextureCookUsage::ColorSrgb,
        "linux",
        crate::TextureCookQuality::Low,
    )
    .expect("cook texture");
    let texture_payload = texture.to_postcard().expect("texture postcard");
    regia_test_utils::write_atomic(
        &cache.join(cooked_file_name(tex_id)),
        &cooked_file(tex_id, regia_asset::AssetTypeTag::texture(), &texture_payload),
    )
    .expect("texture file");

    let mut authored = regia_asset::MaterialAssetRon::default_standard();
    authored.base.base_color_map = Some(regia_asset::TextureSlot::Asset(tex_id));
    let material = regia_asset::CookedStandardMaterial::new(authored);
    let material_payload = material.to_postcard().expect("material postcard");
    regia_test_utils::write_atomic(
        &cache.join(cooked_file_name(mat_id)),
        &cooked_file(
            mat_id,
            regia_asset::AssetTypeTag::standard_material(),
            &material_payload,
        ),
    )
    .expect("material file");
    regia_asset::AssetManifest {
        entries: vec![regia_asset::ManifestEntry {
            path: material_path.to_owned(),
            id: mat_id,
            type_path: "regia_asset::CookedStandardMaterial".to_owned(),
        }],
    }
    .write_to_cache(&cache)
    .expect("manifest");

    let assets = CacheAssets::for_platform(&cache, "linux");
    let records = [
        (1u128, "builtin:cube", 20.0, 4.0),
        (2u128, "builtin:sphere", 40.0, 6.0),
    ]
    .map(|(id, mesh, x, size)| {
        entity(
            id,
            None,
            &[
                (
                    TRANSFORM,
                    format!("(translation: ({x}, {size}, 20.0), scale: ({size}, {size}, {size}))"),
                ),
                (
                    "regia_render::runtime::MeshSource",
                    format!("(mesh: \"{mesh}\", material: \"{material_path}\")"),
                ),
            ],
        )
    });
    let text = format!(
        "(format_version: 1, entities: [{}], resources: {{ \"regia_scene::streaming::LevelStreaming\": (enabled: \
         true, cell_size_m: 100.0, load_radius_m: 400.0, unload_radius_m: 500.0, hlod_atlas_px: 8, \
         hlod_atlas_max_px: 128) }})",
        records.concat()
    );
    let scene = SceneFile::from_ron_str(&text).expect("scene");
    let streaming = level_streaming(&scene, &registry())
        .expect("settings")
        .expect("streamed");
    assert!(streaming.hlod_atlas_enabled());
    let part = partition(&scene, streaming, &registry(), &assets).expect("partition");
    eprintln!("bake_image_decodes={}", assets.bake_image_decodes());
    assert_eq!(
        assets.bake_image_decodes(),
        1,
        "each mesh re-decoded the shared base color"
    );
    let maps: Vec<_> = part
        .cells
        .values()
        .flat_map(|cell| &cell.meshes)
        .filter_map(|(_, soup)| soup.material.as_ref())
        .collect();
    assert_eq!(maps.len(), 2, "both meshes carry the baked material");
    let (first, second) = (
        maps[0].base_color_map.as_ref().expect("base color"),
        maps[1].base_color_map.as_ref().expect("base color"),
    );
    assert!(Arc::ptr_eq(first, second), "the decoded image is one allocation");
    let pixel = &first.rgba[..4];
    for (got, want) in pixel.iter().zip([230u8, 10, 10, 255]) {
        assert!((*got as i16 - want as i16).abs() <= 8, "decoded {pixel:?}");
    }

    cook_streamed_level(lvl(), &scene, &registry(), &assets, Some(&out), "a.level.ron")
        .expect("cook")
        .expect("streamed");
    assert_eq!(
        assets.bake_image_decodes(),
        1,
        "the cook decoded the texture again"
    );
    let index = index_in(&out);
    let hlod = index.hlod.as_ref().expect("hlod");
    let tree = hlod.tree(&index.settings.grid().expect("grid")).expect("tree");
    let leaf = tree.leaf_for(CellCoord::new(0, 0)).expect("leaf") as usize;
    let atlas = hlod.atlas(leaf).expect("atlas");
    let (_, cooked) = regia_asset::load_texture_bytes(
        &std::fs::read(out.join(cooked_file_name(atlas.textures[0]))).expect("atlas"),
    )
    .expect("texture");
    let image = regia_render::CompressedTexture::from_cooked(&cooked)
        .expect("ktx2")
        .decode_rgba8()
        .expect("decodes");
    let texels = image.rgba.as_chunks::<4>().0;
    let near = texels
        .iter()
        .filter(|t| {
            (t[0] as i16 - 230).abs() <= 12
                && (t[1] as i16 - 10).abs() <= 12
                && (t[2] as i16 - 10).abs() <= 12
        })
        .count();
    assert!(
        near * 10 >= texels.len() * 8,
        "{near} of {} atlas texels hold the shared red",
        texels.len()
    );
    let _ = std::fs::remove_dir_all(&root);
}
