// SPDX-License-Identifier: MPL-2.0
//! The built-in kinds (mesh, texture, image, material) on the generic asset API (ADR-7256, spec 12
//! §§ 2-3): counted `StrongHandle`s with the same grace frames, `PrepareAsset` + `keep_cpu_data`,
//! the per-type budget over CPU and GPU bytes, and reference cycles in the `assets.handles` report.
//! Every test steps the server by calls: `wait_for_loads` blocks on the load tasks, `update` is one
//! frame. No clock.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use regia_asset::{
    AssetId, AssetLoadError, AssetLoader, AssetServer, AssetServerSettings, AssetTypeTag,
    COOKED_FORMAT_VERSION, CookedHeader, CookedImage, CookedTexture, CpuMesh, LoadContext, LoadSettings,
    LoadState, PrepareAsset, StrongHandle, TextureCookUsage, cooked_cache_dir, cooked_file_name,
    encode_cooked_file, pack_importer_version,
};
use regia_core::id::TypePathHash;

/// Stands in for the renderer's upload: the ids with "GPU buffers", and every free in order.
#[derive(Default)]
struct FakeGpu {
    resident: Mutex<BTreeSet<AssetId>>,
    freed: Mutex<Vec<AssetId>>,
    uploads: Mutex<u32>,
}

struct MeshUpload(Arc<FakeGpu>);

impl PrepareAsset for MeshUpload {
    type Asset = CpuMesh;

    fn prepare(&self, id: AssetId, asset: &CpuMesh) -> Result<u64, String> {
        *self.0.uploads.lock() += 1;
        self.0.resident.lock().insert(id);
        // Twelve bytes a position, four an index: what a vertex + index buffer would hold.
        Ok((asset.positions.len() * 12 + asset.indices.len() * 4) as u64)
    }

    fn unload(&self, id: AssetId) {
        self.0.resident.lock().remove(&id);
        self.0.freed.lock().push(id);
    }
}

struct TextureUpload(Arc<FakeGpu>);

impl PrepareAsset for TextureUpload {
    type Asset = CookedTexture;

    fn prepare(&self, id: AssetId, asset: &CookedTexture) -> Result<u64, String> {
        *self.0.uploads.lock() += 1;
        self.0.resident.lock().insert(id);
        Ok(u64::from(asset.width) * u64::from(asset.height))
    }

    fn unload(&self, id: AssetId) {
        self.0.resident.lock().remove(&id);
        self.0.freed.lock().push(id);
    }
}

struct Project {
    root: PathBuf,
    cache: PathBuf,
}

impl Project {
    fn new(tag: &str) -> Self {
        let root = regia_test_utils::unique_temp_dir(tag);
        let cache = cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("cache dir");
        Self { root, cache }
    }

    fn write(&self, id: AssetId, tag: AssetTypeTag, payload: &[u8], deps: &[AssetId]) {
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: tag.0,
            asset_id: id,
            importer_version: pack_importer_version(1, 0, 0),
            dependencies: deps.to_vec(),
            payload_len: payload.len() as u64,
        };
        let bytes = encode_cooked_file(&header, payload).expect("encode");
        write_atomic(&self.cache.join(cooked_file_name(id)), &bytes);
    }

    fn write_mesh(&self, id: AssetId, triangles: u32) {
        let mut mesh = CpuMesh::default();
        for t in 0..triangles {
            let base = mesh.positions.len() as u32;
            let x = t as f32;
            mesh.positions
                .extend([[x, 0.0, 0.0], [x + 1.0, 0.0, 0.0], [x, 1.0, 0.0]]);
            mesh.normals.extend([[0.0, 0.0, 1.0]; 3]);
            mesh.uvs.extend([[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]]);
            mesh.indices.extend([base, base + 1, base + 2]);
        }
        let payload = mesh.to_postcard().expect("postcard");
        self.write(id, AssetTypeTag::mesh_legacy_cpu(), &payload, &[]);
    }

    fn write_texture(&self, id: AssetId) {
        let texture = CookedTexture {
            width: 4,
            height: 4,
            usage: TextureCookUsage::ColorSrgb,
            ktx2: minimal_ktx2().into(),
        };
        let payload = texture.to_postcard().expect("postcard");
        self.write(id, AssetTypeTag::texture(), &payload, &[]);
    }

    fn write_image(&self, id: AssetId) {
        let image = CookedImage {
            width: 2,
            height: 2,
            rgba8: vec![255; 16],
        };
        let payload = image.to_postcard().expect("postcard");
        self.write(id, AssetTypeTag::image(), &payload, &[]);
    }

    fn server(&self) -> AssetServer {
        AssetServer::open(AssetServerSettings {
            project_root: self.root.clone(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("open")
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).expect("write");
    std::fs::rename(&tmp, path).expect("rename");
}

/// A minimal valid KTX2 blob (one 4×4 BC7 block), so `CookedTexture::validate` accepts it.
fn minimal_ktx2() -> Vec<u8> {
    use ktx2::dfd::{Basic, Block};
    use ktx2::{Format, Header, Index, LevelIndex};
    let (basic, type_size) = Basic::from_format(Format::BC7_SRGB_BLOCK).expect("dfd");
    let dfd_body = Block::Basic(basic).to_vec();
    let dfd_total_len = 4u32 + dfd_body.len() as u32;
    let level_index_len = LevelIndex::LENGTH;
    let dfd_offset = Header::LENGTH + level_index_len;
    let kvd_offset = dfd_offset + dfd_total_len as usize;
    let payload = vec![0u8; 16];
    let level_index = LevelIndex {
        byte_offset: kvd_offset as u64,
        byte_length: payload.len() as u64,
        uncompressed_byte_length: payload.len() as u64,
    };
    let header = Header {
        format: Some(Format::BC7_SRGB_BLOCK),
        type_size,
        pixel_width: 4,
        pixel_height: 4,
        pixel_depth: 0,
        layer_count: 0,
        face_count: 1,
        level_count: 1,
        supercompression_scheme: None,
        index: Index {
            dfd_byte_offset: dfd_offset as u32,
            dfd_byte_length: dfd_total_len,
            kvd_byte_offset: kvd_offset as u32,
            kvd_byte_length: 0,
            sgd_byte_offset: 0,
            sgd_byte_length: 0,
        },
    };
    let mut out = Vec::new();
    out.extend_from_slice(&header.as_bytes());
    out.extend_from_slice(&level_index.as_bytes());
    out.extend_from_slice(&dfd_total_len.to_le_bytes());
    out.extend_from_slice(&dfd_body);
    out.extend_from_slice(&payload);
    out
}

/// Acceptance 1: dropping the last counted handle to a mesh frees its GPU buffers after exactly
/// the grace frames (the `PrepareAsset::unload` hook), and re-acquiring inside the grace keeps it.
/// The slab handle (`meshes().handle_for_id`) the renderer reads resolves while it is loaded.
#[test]
fn dropping_the_last_mesh_handle_frees_its_gpu_buffers_after_exactly_the_grace_frames() {
    let p = Project::new("builtin-mesh-grace");
    let id = AssetId::new_random();
    p.write_mesh(id, 2);
    let mut server = p.server();
    let gpu = Arc::new(FakeGpu::default());
    server.register_prepare(MeshUpload(Arc::clone(&gpu)));
    server.set_unload_grace_frames(3);

    let handle: StrongHandle<CpuMesh> = server.load::<CpuMesh>(id).expect("counted mesh load");
    server.wait_for_loads();
    assert_eq!(server.state(id), LoadState::Loaded);
    let slab = server.meshes().handle_for_id(id).expect("in the built-in slab");
    assert!(gpu.resident.lock().contains(&id), "uploaded once loaded");
    assert_eq!(server.strong_count(id), 1);

    // Re-acquire inside the grace: the mesh stays, and nothing is uploaded again.
    drop(handle);
    server.update();
    server.update();
    assert!(server.is_pending_unload(id));
    let again = server.load::<CpuMesh>(id).expect("again");
    for _ in 0..10 {
        server.update();
    }
    assert!(gpu.resident.lock().contains(&id));
    assert_eq!(*gpu.uploads.lock(), 1, "kept, not re-read");
    assert!(server.meshes().get(&slab).is_some());

    // Drop for good: still resident after grace - 1 frames, freed on exactly the grace-th.
    drop(again);
    server.update();
    server.update();
    assert!(gpu.resident.lock().contains(&id), "two of three grace frames");
    assert!(server.meshes().get(&slab).is_some());
    server.update();
    assert!(!gpu.resident.lock().contains(&id), "freed on the third frame");
    assert_eq!(*gpu.freed.lock(), vec![id]);
    assert!(
        server.meshes().get(&slab).is_none(),
        "the slab handle stops resolving"
    );
    assert_eq!(server.state(id), LoadState::NotLoaded);
}

/// The migration path: a legacy `load_mesh` (an uncounted `Handle<CpuMesh>`) pins the mesh, so a
/// counted handle taken and dropped beside it never unloads it, and the legacy handle keeps
/// resolving. A counted load of an id the legacy path loaded does not read it again.
#[test]
fn a_legacy_load_pins_the_mesh_and_counted_handles_never_unload_it() {
    let p = Project::new("builtin-mesh-pin");
    let id = AssetId::new_random();
    p.write_mesh(id, 1);
    let mut server = p.server();
    let gpu = Arc::new(FakeGpu::default());
    server.register_prepare(MeshUpload(Arc::clone(&gpu)));
    server.set_unload_grace_frames(2);

    let legacy = server.load_mesh(id).expect("legacy load");
    assert!(server.is_pinned(id));
    let counted = server.load::<CpuMesh>(id).expect("counted");
    server.wait_for_loads();
    assert_eq!(server.strong_count(id), 1);
    drop(counted);
    for _ in 0..10 {
        server.update();
    }
    assert!(!server.is_pending_unload(id));
    assert!(
        server.meshes().get(&legacy).is_some(),
        "the legacy handle still resolves"
    );
    assert!(gpu.freed.lock().is_empty());
    let report = server.handle_report(Some("mesh"));
    let row = report.assets.iter().find(|r| r.id == id).expect("listed");
    assert!(row.pinned);
    assert_eq!(report.kinds[0].pinned, 1);
}

/// Acceptance 2: a texture loaded without `keep_cpu_data` holds no CPU bytes after its upload; one
/// loaded with it does. Meshes follow the same rule.
#[test]
fn a_texture_without_keep_cpu_data_holds_no_cpu_bytes_after_upload_and_one_with_it_does() {
    let p = Project::new("builtin-keep-cpu");
    let (dropped, kept, mesh_dropped, mesh_kept) = (
        AssetId::new_random(),
        AssetId::new_random(),
        AssetId::new_random(),
        AssetId::new_random(),
    );
    p.write_texture(dropped);
    p.write_texture(kept);
    p.write_mesh(mesh_dropped, 4);
    p.write_mesh(mesh_kept, 4);
    let mut server = p.server();
    let gpu = Arc::new(FakeGpu::default());
    server.register_prepare(TextureUpload(Arc::clone(&gpu)));
    server.register_prepare(MeshUpload(Arc::clone(&gpu)));

    let a = server.load::<CookedTexture>(dropped).expect("load");
    let b = server
        .load_with::<CookedTexture>(kept, LoadSettings { keep_cpu_data: true })
        .expect("load kept");
    let c = server.load::<CpuMesh>(mesh_dropped).expect("mesh");
    let d = server
        .load_with::<CpuMesh>(mesh_kept, LoadSettings { keep_cpu_data: true })
        .expect("mesh kept");
    server.wait_for_loads();

    let ta = server.get(&a).expect("texture a");
    assert!(ta.ktx2.is_empty(), "no CPU bytes after the upload");
    assert_eq!((ta.width, ta.height), (4, 4), "the description stays");
    assert_eq!(server.cpu_data_kept(dropped), Some(false));
    let tb = server.get(&b).expect("texture b");
    assert_eq!(&tb.ktx2[..], &minimal_ktx2()[..], "kept for a CPU reader");
    assert_eq!(server.cpu_data_kept(kept), Some(true));
    assert_eq!(server.memory_use::<CookedTexture>(), minimal_ktx2().len() as u64);
    assert_eq!(server.gpu_memory_use::<CookedTexture>(), 32);

    let mc = server.get(&c).expect("mesh c");
    assert!(mc.positions.is_empty() && mc.indices.is_empty());
    let md = server.get(&d).expect("mesh d");
    assert_eq!(md.indices.len(), 12);
    assert_eq!(md.positions.len(), 12);
}

/// Acceptance 3: the per-type budget counts built-in kinds. Textures whose CPU copy was released
/// still count their GPU bytes (one figure per byte: CPU plus GPU, never both for the same copy),
/// and released ones are evicted oldest first.
#[test]
fn the_budget_evicts_released_textures_by_their_gpu_and_cpu_bytes() {
    let p = Project::new("builtin-budget");
    let ids: Vec<AssetId> = (0..3).map(|_| AssetId::new_random()).collect();
    for id in &ids {
        p.write_texture(*id);
    }
    let mut server = p.server();
    let gpu = Arc::new(FakeGpu::default());
    server.register_prepare(TextureUpload(Arc::clone(&gpu)));
    server.set_unload_grace_frames(1000);
    let mut handles: Vec<Option<StrongHandle<CookedTexture>>> = ids
        .iter()
        .map(|id| Some(server.load::<CookedTexture>(*id).expect("load")))
        .collect();
    server.wait_for_loads();
    // Sixteen GPU bytes each, no CPU bytes left.
    assert_eq!(server.memory_use::<CookedTexture>(), 0);
    assert_eq!(server.budgeted_memory_use::<CookedTexture>(), 48);

    for i in [1, 0] {
        handles[i] = None;
        server.update();
    }
    server.set_memory_budget::<CookedTexture>(Some(20));
    server.update();
    assert_eq!(server.take_budget_evictions(), vec![ids[1], ids[0]]);
    assert_eq!(
        server.state(ids[2]),
        LoadState::Loaded,
        "a held texture is never evicted"
    );
    assert_eq!(
        *gpu.freed.lock(),
        vec![ids[1], ids[0]],
        "eviction frees the GPU copy"
    );
    assert_eq!(server.budgeted_memory_use::<CookedTexture>(), 16);
    let report = server.handle_report(Some("texture"));
    assert_eq!(report.kinds[0].budget, Some(20));
    assert_eq!(report.kinds[0].gpu_bytes, 16);
    drop(handles);
}

/// `assets.handles` lists the built-in kinds that hold something (mesh, image) beside the
/// registered ones, and only those: a kind with nothing loaded stays out of the report.
#[test]
fn the_handle_report_lists_built_in_kinds_that_hold_assets() {
    let p = Project::new("builtin-report");
    let (mesh, image) = (AssetId::new_random(), AssetId::new_random());
    p.write_mesh(mesh, 1);
    p.write_image(image);
    let mut server = p.server();
    server.set_unload_grace_frames(4);
    let hm = server.load::<CpuMesh>(mesh).expect("mesh");
    let _legacy_image = server.load_image(image).expect("image");
    server.wait_for_loads();
    drop(hm);
    server.update();

    let report = server.handle_report(None);
    let names: Vec<&str> = report.kinds.iter().map(|k| k.kind.as_str()).collect();
    assert_eq!(names, vec!["image", "mesh"]);
    let mesh_row = report.assets.iter().find(|r| r.id == mesh).expect("mesh row");
    assert_eq!((mesh_row.strong, mesh_row.unload_in_frames), (0, Some(3)));
    assert!(!mesh_row.pinned);
    assert!(mesh_row.cpu_bytes > 0, "no preparer: the CPU copy stays");
    let image_row = report.assets.iter().find(|r| r.id == image).expect("image row");
    assert!(image_row.pinned);
    assert_eq!(image_row.cpu_bytes, 16);
    assert!(server.handle_report(Some("texture")).kinds.is_empty());
    assert!(server.handle_report(Some("material")).kinds.is_empty());
}

const LINK_TYPE: &str = "asset_builtin_handles_test::Link";

/// A test asset that names other links as dependencies.
struct Link {
    _deps: Vec<StrongHandle<Link>>,
}

struct LinkLoader;

impl AssetLoader for LinkLoader {
    type Asset = Link;

    fn type_tag(&self) -> AssetTypeTag {
        AssetTypeTag(TypePathHash::of(LINK_TYPE))
    }

    fn load(&self, payload: &[u8], ctx: &mut LoadContext<'_>) -> Result<Link, AssetLoadError> {
        let text = std::str::from_utf8(payload).map_err(|e| AssetLoadError::Invalid(e.to_string()))?;
        let mut deps = Vec::new();
        for line in text.lines().filter(|l| !l.is_empty()) {
            let id: AssetId = line
                .parse()
                .map_err(|_| AssetLoadError::Invalid(format!("bad id {line:?}")))?;
            deps.push(ctx.load::<Link>(id)?);
        }
        Ok(Link { _deps: deps })
    }
}

/// Acceptance 4: a two-asset cycle (A needs B, B needs A) is reported as a warning in
/// `assets.handles`; an acyclic chain beside it is not.
#[test]
fn a_two_asset_reference_cycle_is_reported() {
    let p = Project::new("builtin-cycle");
    let (a, b, c, d) = (
        AssetId::new_random(),
        AssetId::new_random(),
        AssetId::new_random(),
        AssetId::new_random(),
    );
    let tag = AssetTypeTag(TypePathHash::of(LINK_TYPE));
    p.write(a, tag, b.to_string().as_bytes(), &[b]);
    p.write(b, tag, a.to_string().as_bytes(), &[a]);
    p.write(c, tag, d.to_string().as_bytes(), &[d]);
    p.write(d, tag, b"", &[]);
    let mut server = p.server();
    server.register_loader(LinkLoader);
    let ha = server.load::<Link>(a).expect("a");
    let hc = server.load::<Link>(c).expect("c");
    server.wait_for_loads();
    assert_eq!(server.dependencies_of(a), vec![b]);
    assert_eq!(server.dependencies_of(b), vec![a]);

    let mut expected = vec![a, b];
    expected.sort();
    assert_eq!(server.reference_cycles(), vec![expected.clone()]);
    let report = server.handle_report(None);
    assert_eq!(report.cycles, vec![expected]);
    drop((ha, hc));
}
