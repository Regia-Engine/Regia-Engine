// SPDX-License-Identifier: MPL-2.0
//! Terrain scatter in the HLOD cook (ADR-3253, spec 78 § 11.6): a synthetic forest (no external
//! assets) on a 4 x 4-cell world, one cell with an authored mesh and the rest scatter only.

use super::scatter::{StreamScatter, WorldSoup, gather};
use super::*;
use regia_core::id::Guid;
use regia_render::visbuffer::VoxelFarProxy;
use regia_terrain::scatter::{ScatterClass, ScatterMeshRef, ScatterSet};
use regia_test_utils::unique_temp_dir;
use regia_world::hlod_voxels::{HlodVoxels, hlod_voxels_id};

const TREE: &str = "trees/oak.gltf";

fn lvl() -> LevelId {
    LevelId(Guid::from_u128(0x3253))
}

fn registry() -> TypeRegistry {
    let mut registry = TypeRegistry::default();
    register_cook_types(&mut registry);
    registry
}

/// A tree of about 300 triangles, pivot at the base: a 4 m trunk and a 2.5 m crown at 5.5 m.
pub(super) fn tree() -> MeshSoup {
    let (trunk, trunk_indices) = regia_render::primitives::box_mesh([0.3, 2.0, 0.3]);
    let (crown, crown_indices) = regia_render::uv_sphere(2.5, 12, 8);
    let mut out = MeshSoup::default();
    for (vertices, indices, lift, color) in [
        (trunk, trunk_indices, 2.0, [0.25, 0.16, 0.08, 1.0]),
        (crown, crown_indices, 5.5, [0.12, 0.3, 0.08, 1.0]),
    ] {
        let base = out.positions.len() as u32;
        for v in &vertices {
            out.positions
                .push([v.position[0], v.position[1] + lift, v.position[2]]);
            out.normals.push(v.normal);
            out.uvs.push(v.uv);
            out.colors.push(color);
        }
        out.indices.extend(indices.iter().map(|i| i + base));
    }
    out
}

/// A forest over a 400 m terrain whose corner is at the world origin (cells 0..=3 on both axes at
/// 100 m), on a gentle slope; `density` trees per 20 m class cell.
pub(super) fn forest(density: f32) -> StreamScatter {
    let n = 33u32;
    let heights = (0..n * n).map(|i| (i % n) as f32 * 0.25).collect();
    let map = regia_terrain::TerrainMap::from_heights(n, n, heights, 400.0, 1.0, 0.0).expect("map");
    let set = ScatterSet {
        seed: 77,
        classes: vec![ScatterClass {
            name: "trees".into(),
            meshes: vec![ScatterMeshRef {
                mesh: TREE.into(),
                weight: 1.0,
                min_scale: 0.9,
                max_scale: 1.3,
                scale_bias: 1.0,
            }],
            cell_size: 20.0,
            density,
            ..ScatterClass::default()
        }],
        ..ScatterSet::default()
    };
    let mut scatter = StreamScatter::from_cooked(&set.to_cooked().expect("cooked set"), map).expect("set");
    scatter.origin = [200.0, 0.0, 200.0];
    scatter
}

/// The cook's assets: the tree mesh by path, and the given scatter.
pub(super) struct Forest(pub(super) Vec<StreamScatter>);

impl StreamAssets for Forest {
    fn cost(&self, _id: AssetId) -> Option<AssetCost> {
        None
    }

    fn mesh(&self, path: &str) -> Option<MeshSoup> {
        (path == TREE).then(tree)
    }

    fn scatter(&self) -> &[StreamScatter] {
        &self.0
    }
}

/// A streamed level with HLOD on and one cube in cell (0, 0): every other cell is scatter only.
pub(super) fn level() -> SceneFile {
    let text = "(format_version: 1, entities: [(guid: \"00000000-0000-0000-0000-000000000001\", name: \"rock\", \
                components: { \"regia_scene::transform::Transform\": (translation: (50.0, 0.0, 50.0)), \
                \"regia_render::runtime::MeshSource\": (mesh: \"builtin:cube\", material: \"\") })], resources: { \
                \"regia_scene::streaming::LevelStreaming\": (enabled: true, cell_size_m: 100.0, load_radius_m: \
                250.0, unload_radius_m: 300.0) })";
    SceneFile::from_ron_str(text).expect("level")
}

fn cook(assets: &dyn StreamAssets, dir: &Path) -> StreamCookOutput {
    cook_streamed_level(
        lvl(),
        &level(),
        &registry(),
        assets,
        Some(dir),
        "maps/forest.level.ron",
    )
    .expect("cook")
    .expect("streamed")
}

fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dir)
        .expect("dir")
        .filter_map(Result::ok)
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read(e.path()).expect("file"),
            )
        })
        .collect()
}

fn read<T>(dir: &Path, id: AssetId, decode: impl FnOnce(&[u8]) -> T) -> T {
    decode(&std::fs::read(dir.join(cooked_file_name(id))).expect("cooked file"))
}

#[test]
fn a_cell_with_scatter_and_no_meshes_gets_a_non_empty_voxel_proxy() {
    let dir = unique_temp_dir("hlod_scatter_voxels");
    let output = cook(&Forest(vec![forest(2.0)]), &dir);
    let index = read(&dir, world_index_id(lvl()), |b| {
        WorldIndex::from_bytes(b, lvl()).expect("index")
    });
    let hlod = index.hlod.as_ref().expect("hlod");
    let tree = hlod.tree(&index.settings.grid().expect("grid")).expect("tree");
    // Only cell (0, 0) has a record; all 16 cells are leaves of the tree.
    assert_eq!(index.cells.len(), 1);
    assert_eq!(hlod.proxies.len(), 16);
    let table = read(&dir, hlod_voxels_id(lvl()), |b| {
        HlodVoxels::from_bytes(b, lvl()).expect("table")
    });
    assert_eq!(
        table.count(),
        tree.nodes().len(),
        "every node has scatter under it"
    );
    assert_eq!(output.report.voxel_proxies, tree.nodes().len());
    assert!(
        output.report.scatter_instances > 16 * 20,
        "{}",
        output.report.scatter_instances
    );
    let leaf = tree.leaf_for(CellCoord::new(2, 2)).expect("leaf") as usize;
    assert_eq!(
        hlod.node_proxies[leaf], None,
        "a scatter-only cell has no mesh proxy"
    );
    let node = table.node(leaf).expect("voxel proxy");
    assert!(node.instances > 20 && node.voxels > 0, "{node:?}");
    let proxy = read(&dir, node.proxy, |b| {
        VoxelFarProxy::from_cooked_file(b).expect("proxy")
    });
    assert_eq!(proxy.voxel_count(0), node.voxels as usize);
    assert!(regia_render::hlod_voxel_mesh(&proxy).is_some_and(|m| !m.indices.is_empty()));
    // The leaf's height range covers its trees (crowns top out near 8 m times the scale).
    let cell = hlod
        .proxies
        .iter()
        .find(|p| p.cell == CellCoord::new(2, 2))
        .expect("leaf input");
    assert!(cell.max_y > 7.0 && cell.min_y < 30.0, "{cell:?}");
    // Coarser nodes have coarser voxels.
    let root = tree.roots()[0] as usize;
    let root_node = table.node(root).expect("root voxels");
    assert!(
        root_node.voxel_size_m > node.voxel_size_m,
        "{root_node:?} vs {node:?}"
    );
    assert_eq!(root_node.instances as u64, output.report.scatter_instances);
    assert!(
        output
            .manifest
            .iter()
            .any(|m| m.path == "maps/forest.level.ron#hlod_voxels")
    );
}

#[test]
fn cooking_a_forest_twice_gives_identical_bytes() {
    let (a, b) = (
        unique_temp_dir("hlod_scatter_a"),
        unique_temp_dir("hlod_scatter_b"),
    );
    let first = cook(&Forest(vec![forest(2.0)]), &a);
    cook(&Forest(vec![forest(2.0)]), &b);
    assert_eq!(files(&a), files(&b));
    // A recook into the same cache writes nothing.
    let before = files(&a);
    let again = cook(&Forest(vec![forest(2.0)]), &a);
    assert_eq!(files(&a), before);
    assert_eq!(again.manifest, first.manifest);
    assert_eq!(
        (
            again.report.voxel_proxies,
            again.report.voxel_bytes,
            again.report.scatter_instances
        ),
        (
            first.report.voxel_proxies,
            first.report.voxel_bytes,
            first.report.scatter_instances
        )
    );
}

/// A recook that leaves no node with voxels removes the voxel table an earlier cook wrote into the
/// same cache: the runtime probes the table by the level's id (`open_streamed_level`), so a stale
/// one would draw the old forest over the new tree. The recook's files match a fresh cook's,
/// apart from the old per-node proxies, which nothing names any more.
#[test]
fn a_recook_without_scatter_removes_the_stale_voxel_table() {
    let (dir, fresh) = (
        unique_temp_dir("hlod_scatter_stale"),
        unique_temp_dir("hlod_scatter_stale_fresh"),
    );
    let table = cooked_file_name(hlod_voxels_id(lvl()));
    cook(&Forest(vec![forest(2.0)]), &dir);
    assert!(
        files(&dir).contains_key(&table),
        "the forest cook writes the table"
    );
    let recook = cook(&Forest(Vec::new()), &dir);
    assert!(
        !files(&dir).contains_key(&table),
        "the stale table is gone after a recook without scatter"
    );
    assert_eq!(recook.report.voxel_proxies, 0);
    // A recook without a table to remove is fine too.
    cook(&Forest(Vec::new()), &dir);
    cook(&Forest(Vec::new()), &fresh);
    let recooked = files(&dir);
    for (name, bytes) in files(&fresh) {
        assert_eq!(recooked.get(&name), Some(&bytes), "{name}");
    }
}

/// ADR-8860 (review w7 finding 5): removing a stale voxel table is a change like a write, so the
/// old world index goes first. A cache that matches this cook apart from a leftover table: a cook
/// stopped right after that removal (the `cook_partition` test hook) leaves no index, and the
/// complete recook writes it again.
#[test]
fn removing_a_stale_voxel_table_drops_the_old_index_first() {
    let dir = unique_temp_dir("hlod_scatter_stale_index");
    cook(&(), &dir);
    let index = dir.join(cooked_file_name(world_index_id(lvl())));
    let table = dir.join(cooked_file_name(hlod_voxels_id(lvl())));
    assert!(index.is_file() && !table.exists());
    std::fs::write(&table, b"a table an earlier cook left").expect("stale table");
    let writes = test_hooks::file_writes();
    test_hooks::stop_after_writes(Some(1));
    let stopped = cook_streamed_level(
        lvl(),
        &level(),
        &registry(),
        &(),
        Some(&dir),
        "maps/forest.level.ron",
    );
    test_hooks::stop_after_writes(None);
    assert!(
        stopped.is_err(),
        "the stale table's removal is the cook's one change, and the hook stops after it"
    );
    assert_eq!(test_hooks::file_writes() - writes, 1);
    assert!(!table.exists(), "the stale table is gone");
    assert!(
        !index.exists(),
        "the old index was removed before the first change"
    );
    cook(&(), &dir);
    assert!(index.is_file(), "the complete recook writes the index again");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_world_without_scatter_cooks_exactly_as_with_no_scatter_support() {
    let (a, b, c) = (
        unique_temp_dir("hlod_noscatter_a"),
        unique_temp_dir("hlod_noscatter_b"),
        unique_temp_dir("hlod_noscatter_c"),
    );
    let plain = cook(&(), &a);
    let empty = cook(&Forest(Vec::new()), &b);
    // A scatter whose every candidate is thinned out places nothing.
    let mut bare = forest(2.0);
    bare.set.classes[0].density = 0.0;
    let thinned = cook(&Forest(vec![bare]), &c);
    assert_eq!(files(&a), files(&b));
    assert_eq!(files(&a), files(&c));
    assert_eq!(plain.manifest, empty.manifest);
    assert_eq!(plain.manifest, thinned.manifest);
    assert!(!files(&a).contains_key(&cooked_file_name(hlod_voxels_id(lvl()))));
    assert_eq!(plain.report.voxel_proxies, 0);
    // The pinned hash of this cook as the base wrote it (before ADR-3253): measured on the
    // base tree (044ead045) and this branch, identical on both. It pins that ADR-3253 did
    // not change any byte a world without scatter cooks.
    let mut hasher = blake3::Hasher::new();
    for (name, bytes) in files(&a) {
        hasher.update(name.as_bytes());
        hasher.update(&bytes);
    }
    let hash = hasher.finalize().to_hex();
    eprintln!("no-scatter cook hash: {hash}");
    assert_eq!(
        hash.as_str(),
        "4837211cc60b17621e8e9ddae862711107b41f8162f1c73b779393b422d73f42"
    );
}

#[test]
fn instances_land_in_the_cell_of_their_ground_point_and_stand_on_the_terrain() {
    let scatter = vec![forest(2.0)];
    let grid = regia_scene::stream_grid::StreamGrid::new(100.0).expect("grid");
    let cells = gather(&scatter, 32, &grid, &|path| (path == TREE).then(tree));
    assert!(cells.warnings.is_empty(), "{:?}", cells.warnings);
    assert_eq!(cells.cells.len(), 16);
    for (coord, cell) in &cells.cells {
        let lo = [coord.x as f64 * 100.0, coord.z as f64 * 100.0];
        // Every trunk base (the lowest vertices) is inside the cell or within a crown of it.
        for p in &cell.soup.positions {
            assert!(p[0] > lo[0] - 5.0 && p[0] < lo[0] + 105.0, "{coord}: {p:?}");
            assert!(p[2] > lo[1] - 5.0 && p[2] < lo[1] + 105.0, "{coord}: {p:?}");
        }
        // The terrain rises 0.25 m per sample (12.5 m apart) along x: trees follow it.
        let ground = (lo[0] + 50.0) / 12.5 * 0.25;
        assert!(
            cell.min_y > ground - 4.0 && cell.min_y < ground + 2.5,
            "{coord}: {}",
            cell.min_y
        );
        assert_eq!(cell.soup.positions.len(), cell.soup.colors.len());
        assert_eq!(cell.soup.positions.len(), cell.soup.uvs.len());
    }
    // An unknown mesh is a warning, not a failure.
    let none = gather(&scatter, 32, &grid, &|_| None);
    assert!(none.cells.is_empty());
    assert_eq!(none.warnings[0].code, "stream_hlod_scatter_mesh");
}

/// deep-zai-terrain-stream #12: instances whose ground point is outside the addressable cell range
/// (a mis-authored origin) are counted and reported, not dropped in silence.
#[test]
fn instances_outside_the_cell_range_are_reported() {
    let mut far = forest(2.0);
    far.origin = [1.0e22, 0.0, 1.0e22];
    let grid = regia_scene::stream_grid::StreamGrid::new(100.0).expect("grid");
    let cells = gather(&[far], 32, &grid, &|path| (path == TREE).then(tree));
    assert!(cells.cells.is_empty());
    let warning = cells
        .warnings
        .iter()
        .find(|w| w.code == "stream_hlod_scatter_failed")
        .unwrap_or_else(|| panic!("no warning for the dropped instances: {:?}", cells.warnings));
    assert!(
        warning.message.contains("outside the cell range"),
        "{}",
        warning.message
    );
}

/// The coverage a soup leaves on a fixed far view: an unlit red draw against the empty frame,
/// lavapipe readback.
fn coverage(
    device: &regia_render::RenderDevice,
    mesh: Option<(Vec<regia_render::LabVertex>, Vec<u32>)>,
) -> Vec<bool> {
    use regia_render::{DirectionalLightUniform, LabMaterial, LabMesh, LabScene};
    let mut meshes = vec![LabMesh {
        // A speck far behind the camera keeps the mesh list non-empty.
        vertices: vec![
            regia_render::LabVertex {
                position: [0.0, -500.0, 900.0],
                normal: [0.0, 1.0, 0.0],
                uv: [0.0; 2],
                tangent: [1.0, 0.0, 0.0, 1.0],
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            };
            3
        ],
        indices: vec![0, 1, 2],
        material: 0,
        ..LabMesh::unskinned()
    }];
    if let Some((vertices, indices)) = mesh {
        meshes.push(LabMesh {
            vertices,
            indices,
            material: 0,
            ..LabMesh::unskinned()
        });
    }
    let scene = LabScene {
        width: COVERAGE_W,
        height: COVERAGE_H,
        // 450 m from the cell center, 30 degrees above it.
        eye: [250.0, 225.0 + 6.0, 250.0 + 390.0],
        target: [250.0, 6.0, 250.0],
        fov_y: 20.0_f32.to_radians(),
        near: 1.0,
        far: 2000.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 14.8,
        ambient_lux: 0.0,
        double_sided: true,
        post: None,
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: vec![],
        materials: vec![LabMaterial {
            base_color: [1.0, 0.0, 0.0, 1.0],
            unlit: true,
            ..LabMaterial::default()
        }],
        meshes,
    };
    let frame = regia_render::render_lab_scene(device, &scene).expect("render");
    let empty = coverage_reference(device, &scene);
    frame
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .zip(empty.as_chunks::<4>().0)
        .map(|(a, b)| (0..3).any(|k| a[k].abs_diff(b[k]) > 24))
        .collect()
}

fn coverage_reference(device: &regia_render::RenderDevice, scene: &regia_render::LabScene) -> Vec<u8> {
    let mut empty = scene.clone();
    empty.meshes.truncate(1);
    regia_render::render_lab_scene(device, &empty)
        .expect("render")
        .pixels
}

const COVERAGE_W: u32 = 256;
const COVERAGE_H: u32 = 160;

fn lab_vertices(soup: &WorldSoup) -> (Vec<regia_render::LabVertex>, Vec<u32>) {
    let vertices = soup
        .positions
        .iter()
        .zip(&soup.normals)
        .map(|(p, n)| regia_render::LabVertex {
            position: [p[0] as f32, p[1] as f32, p[2] as f32],
            normal: *n,
            uv: [0.0; 2],
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect();
    (vertices, soup.indices.clone())
}

/// Pixels of `a` with no pixel of `b` within `bound` (Chebyshev).
fn outside(a: &[bool], b: &[bool], bound: i32) -> usize {
    let (w, h) = (COVERAGE_W as i32, COVERAGE_H as i32);
    let at = |x: i32, y: i32| x >= 0 && y >= 0 && x < w && y < h && b[(y * w + x) as usize];
    (0..h)
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .filter(|&(x, y)| a[(y * w + x) as usize])
        .filter(|&(x, y)| !(-bound..=bound).any(|dy| (-bound..=bound).any(|dx| at(x + dx, y + dy))))
        .count()
}

/// The far view of cell (2, 2): its voxel proxy covers the screen its trees cover, within a stated
/// bound. The proxy's boxes enclose the surface, so every tree pixel is a proxy pixel within one
/// pixel (rasterization), and every proxy pixel lies within a voxel diagonal of a tree pixel
/// (`VOXEL_DIAGONAL_PX`, the diagonal of a 100 m / 32 voxel at 450 m is about 4.6 px here). The
/// covered area is at least the trees' and at most the trees' grown by one voxel edge
/// (`VOXEL_EDGE_PX`, about 3.4 px here): a box touches the surface in its voxel on every face, so
/// it reaches at most one voxel edge past it. Measured on lavapipe: 1178 tree pixels, 2025 proxy
/// pixels (1.72 times; sparse crowns are mostly silhouette, so the edge growth dominates).
#[test]
fn a_voxel_proxy_covers_the_far_view_its_trees_cover() {
    const VOXEL_DIAGONAL_PX: i32 = 5;
    const VOXEL_EDGE_PX: i32 = 4;
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip hlod scatter coverage: {err}");
            return;
        }
    };
    let scatter = vec![forest(2.0)];
    let grid = regia_scene::stream_grid::StreamGrid::new(100.0).expect("grid");
    let cells = gather(&scatter, 32, &grid, &|path| (path == TREE).then(tree));
    let cell = &cells.cells[&CellCoord::new(2, 2)];
    let proxy = super::scatter::voxelize("test", &cell.soup, [0.0; 3], None, 32).expect("voxelize");
    let mesh = regia_render::hlod_voxel_mesh(&proxy).expect("surface");
    let full = coverage(&device, Some(lab_vertices(&cell.soup)));
    let voxels = coverage(&device, Some((mesh.vertices, mesh.indices)));
    let (full_px, voxel_px) = (
        full.iter().filter(|c| **c).count(),
        voxels.iter().filter(|c| **c).count(),
    );
    let ratio = voxel_px as f32 / full_px.max(1) as f32;
    let (missed, stray) = (
        outside(&full, &voxels, 1),
        outside(&voxels, &full, VOXEL_DIAGONAL_PX),
    );
    eprintln!(
        "hlod scatter coverage: trees {full_px} px, voxels {voxel_px} px (ratio {ratio:.3}), tree pixels \
         uncovered {missed}, voxel pixels past the bound {stray}"
    );
    assert!(full_px > 200, "the trees are on screen ({full_px} px)");
    assert_eq!(missed, 0, "the proxy covers every tree pixel");
    assert_eq!(stray, 0, "no proxy pixel lies past a voxel diagonal of the trees");
    // The trees' mask grown by one voxel edge: every pixel within `VOXEL_EDGE_PX` of a tree pixel.
    let everywhere = vec![true; full.len()];
    let grown = full.len() - outside(&everywhere, &full, VOXEL_EDGE_PX);
    eprintln!("hlod scatter coverage: trees grown by one voxel edge {grown} px");
    assert!(voxel_px >= full_px, "the proxy covers at least the trees' area");
    assert!(
        voxel_px <= grown,
        "coverage {voxel_px} px past the one-edge growth {grown} px"
    );
}

/// The 8 x 8-cell generated forest of `docs/benchmark-scenes.md` (§ HLOD scatter voxel proxies):
/// 128 m cells over a 1024 m terrain, two tree sizes, no external assets. Prints the cook-size and
/// draw table; timings are printed, never asserted. Run:
/// `cargo test -p regia_asset_pipeline --release --lib hlod_scatter_bench -- --ignored --nocapture`.
#[test]
#[ignore = "benchmark: prints the table in docs/benchmark-scenes.md"]
fn hlod_scatter_bench() {
    let n = 65u32;
    let heights = (0..n * n)
        .map(|i| {
            let (x, z) = ((i % n) as f32, (i / n) as f32);
            (x * 0.21).sin() * 6.0 + (z * 0.17).cos() * 5.0
        })
        .collect();
    let map = regia_terrain::TerrainMap::from_heights(n, n, heights, 1024.0, 1.0, 0.0).expect("map");
    let class = |name: &str, density: f32, scale: (f32, f32)| ScatterClass {
        name: name.into(),
        meshes: vec![ScatterMeshRef {
            mesh: TREE.into(),
            weight: 1.0,
            min_scale: scale.0,
            max_scale: scale.1,
            scale_bias: 1.0,
        }],
        cell_size: 16.0,
        density,
        noise_scale: 90.0,
        noise_amount: 0.6,
        ..ScatterClass::default()
    };
    let set = ScatterSet {
        seed: 2026,
        classes: vec![class("oaks", 1.5, (0.9, 1.4)), class("saplings", 1.0, (0.4, 0.7))],
        ..ScatterSet::default()
    };
    let mut forest = StreamScatter::from_cooked(&set.to_cooked().expect("set"), map).expect("forest");
    forest.origin = [512.0, 0.0, 512.0];
    let text = "(format_version: 1, entities: [(guid: \"00000000-0000-0000-0000-000000000001\", name: \"rock\", \
                components: { \"regia_scene::transform::Transform\": (translation: (64.0, 0.0, 64.0)), \
                \"regia_render::runtime::MeshSource\": (mesh: \"builtin:cube\", material: \"\") })], resources: { \
                \"regia_scene::streaming::LevelStreaming\": (enabled: true, cell_size_m: 128.0, load_radius_m: \
                384.0, unload_radius_m: 448.0) })";
    let scene = SceneFile::from_ron_str(text).expect("level");
    for resolution in [16u32, 32, 64] {
        let dir = unique_temp_dir("hlod_scatter_bench");
        let assets = WithScatter {
            assets: &Forest(Vec::new()),
            scatter: vec![forest.clone()],
            resolution,
        };
        let start = std::time::Instant::now();
        let output = cook_streamed_level(lvl(), &scene, &registry(), &assets, Some(&dir), "bench.level.ron")
            .expect("cook")
            .expect("streamed");
        let seconds = start.elapsed().as_secs_f64();
        let index = read(&dir, world_index_id(lvl()), |b| {
            WorldIndex::from_bytes(b, lvl()).expect("index")
        });
        let hlod = index.hlod.as_ref().expect("hlod");
        let tree = hlod.tree(&index.settings.grid().expect("grid")).expect("tree");
        let table = read(&dir, hlod_voxels_id(lvl()), |b| {
            HlodVoxels::from_bytes(b, lvl()).expect("table")
        });
        let mut by_level: BTreeMap<u32, (u32, u64, u64, f32)> = BTreeMap::new();
        let mut faces = vec![0usize; tree.nodes().len()];
        for (i, node) in tree.nodes().iter().enumerate() {
            let Some(v) = table.node(i) else { continue };
            let proxy = read(&dir, v.proxy, |b| {
                VoxelFarProxy::from_cooked_file(b).expect("proxy")
            });
            faces[i] = regia_render::hlod_voxel_mesh(&proxy).map_or(0, |m| m.indices.len() / 3);
            let row = by_level.entry(node.level).or_insert((0, 0, 0, v.voxel_size_m));
            row.0 += 1;
            row.1 += v.bytes;
            row.2 += u64::from(v.voxels);
        }
        println!(
            "resolution {resolution}: cook {seconds:.2} s, {} instances, {} voxel proxies, {} voxel bytes, {} nodes",
            output.report.scatter_instances,
            output.report.voxel_proxies,
            output.report.voxel_bytes,
            tree.nodes().len()
        );
        for (level, (nodes, bytes, voxels, size)) in &by_level {
            println!(
                "  level {level}: {nodes} nodes, {:.1} KiB per node, {} voxels per node, voxel {size:.2} m",
                *bytes as f64 / f64::from(*nodes) / 1024.0,
                voxels / u64::from(*nodes)
            );
        }
        // The cut from three distances west of the world, on its center line.
        for distance in [256.0f64, 1024.0, 4096.0] {
            let viewer = [-distance, 20.0, 512.0];
            let cut = tree.select(&[viewer]).expect("select");
            let (mut proxies, mut full, mut triangles) = (0, 0, 0usize);
            for draw in &cut {
                match draw.kind {
                    regia_scene::hlod::HlodDrawKind::Proxy => {
                        proxies += 1;
                        triangles += faces[draw.node as usize];
                    }
                    regia_scene::hlod::HlodDrawKind::Full => full += 1,
                }
            }
            println!(
                "  viewer {distance} m out: {proxies} voxel proxies ({triangles} voxel-box triangles), {full} full cells"
            );
        }
    }
}

/// SC2 (ADR-8496): the HLOD cook places a set under its `placement_origin`, so a world-anchored
/// set's proxies hold the world-anchored instances (the ones the runtime field draws), cell by cell;
/// the default stays the map-edge placement.
#[test]
fn a_world_anchored_set_gathers_the_world_anchored_instances() {
    use regia_terrain::scatter::placement::{
        PlacementInputs, PlacementOrigin, WorldAnchor, place_set_with_origin,
    };
    let grid = regia_scene::stream_grid::StreamGrid::new(100.0).expect("grid");
    // Instances per streaming cell of the forest (map centre at world 200, 200) under `origin`.
    let expected = |scatter: &StreamScatter, origin: &PlacementOrigin| {
        let inputs = PlacementInputs {
            map: &scatter.map,
            holes: None,
            layers: None,
            density_maps: None,
        };
        let mut per_cell: BTreeMap<(i64, i64), u32> = BTreeMap::new();
        for p in place_set_with_origin(&scatter.set, inputs, origin).expect("place") {
            let x = ((f64::from(p.position[0]) + 200.0) / 100.0).floor() as i64;
            let z = ((f64::from(p.position[2]) + 200.0) / 100.0).floor() as i64;
            *per_cell.entry((x, z)).or_default() += 1;
        }
        per_cell
    };
    let gathered = |scatter: &StreamScatter| -> BTreeMap<(i64, i64), u32> {
        let cells = gather(std::slice::from_ref(scatter), 32, &grid, &|path| {
            (path == TREE).then(tree)
        });
        assert!(cells.warnings.is_empty(), "{:?}", cells.warnings);
        cells
            .cells
            .iter()
            .map(|(c, cell)| ((c.x, c.z), cell.instances))
            .collect()
    };
    let edge = forest(2.0);
    assert_eq!(edge.placement_origin, PlacementOrigin::MapEdge);
    let edge_cells = gathered(&edge);
    assert_eq!(edge_cells, expected(&edge, &PlacementOrigin::MapEdge));
    let origin = PlacementOrigin::World(WorldAnchor::at([5.0, -7.0]));
    let mut anchored = forest(2.0);
    anchored.placement_origin = origin;
    let anchored_cells = gathered(&anchored);
    assert_eq!(anchored_cells, expected(&anchored, &origin));
    assert_ne!(anchored_cells, edge_cells, "the anchor moves the trees");
}
