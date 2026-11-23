// SPDX-License-Identifier: MPL-2.0
//! The `.meta` fracture cook (ADR-1967): `regia cook` turns a closed glTF into chunk meshes and a
//! destructible asset the runtime can load without the pipeline, keeps them current, and refuses a
//! mesh it cannot fracture.

use std::path::{Path, PathBuf};

use regia_asset::{
    AssetId, AssetManifest, CookedIndexWidth, CookedMesh, MetaSidecar, cooked_cache_dir, cooked_file_name,
    decode_cooked_file, meta_path_for,
};
use regia_asset_pipeline::fracture::box_mesh;
use regia_asset_pipeline::fracture_cook::{FRACTURE_DESTRUCTIBLE_SUB_PATH, fracture_chunk_sub_path};
use regia_asset_pipeline::{CookOptions, CookReport, cook_project, host_platform};
use regia_physics::destruction::{DESTRUCTIBLE_TYPE_PATH, DestructibleAsset};

/// `<root>/assets/wall.glb`: a 24-vertex box with normals and per-face UVs. `open` drops a
/// triangle, so the mesh has a hole.
fn write_glb(root: &Path, open: bool) -> PathBuf {
    let (positions, indices) = box_mesh([1.0, 0.5, 0.25]).expect("box");
    let mut p: Vec<[f32; 3]> = Vec::new();
    let mut n: Vec<[f32; 3]> = Vec::new();
    let mut uv: Vec<[f32; 2]> = Vec::new();
    let mut i: Vec<u16> = Vec::new();
    let tris = indices.as_chunks::<3>().0;
    let tris = if open { &tris[1..] } else { tris };
    for tri in tris {
        let [a, b, c] = tri.map(|v| positions[v as usize]);
        let e = |x: [f32; 3], y: [f32; 3]| [x[0] - y[0], x[1] - y[1], x[2] - y[2]];
        let (u, v) = (e(b, a), e(c, a));
        let f = [
            u[1] * v[2] - u[2] * v[1],
            u[2] * v[0] - u[0] * v[2],
            u[0] * v[1] - u[1] * v[0],
        ];
        let l = (f[0] * f[0] + f[1] * f[1] + f[2] * f[2]).sqrt();
        for q in [a, b, c] {
            i.push(p.len() as u16);
            p.push(q);
            n.push([f[0] / l, f[1] / l, f[2] / l]);
            uv.push([q[0] + q[2], q[1] + 0.5 * q[2]]);
        }
    }
    let mut bin: Vec<u8> = Vec::new();
    for v in p
        .iter()
        .flatten()
        .chain(n.iter().flatten())
        .chain(uv.iter().flatten())
    {
        bin.extend_from_slice(&v.to_le_bytes());
    }
    for v in &i {
        bin.extend_from_slice(&v.to_le_bytes());
    }
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let count = p.len();
    let (pos_len, uv_len) = (count * 12, count * 8);
    let json = format!(
        r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],"nodes":[{{"mesh":0,"name":"Wall"}}],
"meshes":[{{"primitives":[{{"attributes":{{"POSITION":0,"NORMAL":1,"TEXCOORD_0":2}},"indices":3,"mode":4}}]}}],
"buffers":[{{"byteLength":{bin}}}],
"bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":{pos_len}}},{{"buffer":0,"byteOffset":{pos_len},"byteLength":{pos_len}}},
{{"buffer":0,"byteOffset":{uv_at},"byteLength":{uv_len}}},{{"buffer":0,"byteOffset":{idx_at},"byteLength":{idx_len}}}],
"accessors":[{{"bufferView":0,"componentType":5126,"count":{count},"type":"VEC3","min":[-1,-0.5,-0.25],"max":[1,0.5,0.25]}},
{{"bufferView":1,"componentType":5126,"count":{count},"type":"VEC3"}},
{{"bufferView":2,"componentType":5126,"count":{count},"type":"VEC2"}},
{{"bufferView":3,"componentType":5123,"count":{index_count},"type":"SCALAR"}}]}}"#,
        bin = bin.len(),
        uv_at = pos_len * 2,
        idx_at = pos_len * 2 + uv_len,
        idx_len = i.len() * 2,
        index_count = i.len(),
    );
    let mut json = json.into_bytes();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let total = 12 + 8 + json.len() + 8 + bin.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&0x4654_6C67u32.to_le_bytes());
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(&0x4E4F_534Au32.to_le_bytes());
    out.extend_from_slice(&json);
    out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    out.extend_from_slice(&0x004E_4942u32.to_le_bytes());
    out.extend_from_slice(&bin);
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets");
    let path = assets.join("wall.glb");
    std::fs::write(&path, out).expect("glb");
    path
}

fn write_settings(glb: &Path, settings: &str) {
    let meta = format!(
        "(format_version: 1, id: \"11111111-2222-4333-8444-555555555555\", importer: \"gltf\", settings: Some({settings:?}))"
    );
    std::fs::write(meta_path_for(glb), meta).expect("meta");
}

fn cook(root: &Path) -> Result<CookReport, String> {
    let report = cook_project(CookOptions::for_host(root.to_path_buf())).map_err(|e| e.to_string())?;
    if report.errors.is_empty() {
        Ok(report)
    } else {
        Err(format!("{:?}", report.errors))
    }
}

struct Cooked {
    meta: MetaSidecar,
    destructible: AssetId,
    cache: PathBuf,
}

fn cooked(root: &Path, glb: &Path) -> Cooked {
    let meta = MetaSidecar::from_ron(&std::fs::read(meta_path_for(glb)).expect("read meta")).expect("meta");
    let destructible = AssetId::sub_asset(meta.id, FRACTURE_DESTRUCTIBLE_SUB_PATH);
    Cooked {
        meta,
        destructible,
        cache: cooked_cache_dir(root, host_platform()),
    }
}

fn read_asset(c: &Cooked) -> DestructibleAsset {
    let bytes = std::fs::read(c.cache.join(cooked_file_name(c.destructible))).expect("cooked destructible");
    DestructibleAsset::from_cooked_file(&bytes).expect("loads")
}

#[test]
fn a_meta_setting_cooks_chunk_meshes_and_a_runtime_asset() {
    let root = regia_test_utils::unique_temp_dir("fracture-cook");
    let glb = write_glb(&root, false);
    write_settings(
        &glb,
        "(fracture: Some((chunks: 6, seed: 3, density: 1000.0, fx_tag: \"brick\")))",
    );
    cook(&root).expect("cook");
    let c = cooked(&root, &glb);

    // The destructible, loaded the way a game does.
    let asset = read_asset(&c);
    assert_eq!(asset.chunks.len(), 6);
    assert_eq!(asset.material.fx_tag, "brick");
    assert_eq!(asset.fracture_seed, Some(3));
    assert!(asset.meshes.is_none());
    assert!(!asset.bonds.is_empty());
    assert!(asset.chunks.iter().any(|c| c.support));
    let mass: f64 = asset.chunks.iter().map(|c| f64::from(c.mass)).sum();
    assert!((mass - 2.0 * 1.0 * 0.5 * 1000.0).abs() < 1.0, "{mass}");

    // One render mesh per chunk, named by the chunk, with both material slots and full attributes.
    for (i, chunk) in asset.chunks.iter().enumerate() {
        let path = fracture_chunk_sub_path(i);
        assert_eq!(chunk.lods, vec![format!("assets/wall.glb#{path}")]);
        let id = AssetId::sub_asset(c.meta.id, &path);
        let bytes = std::fs::read(c.cache.join(cooked_file_name(id))).expect("cooked chunk");
        let (_, payload) = decode_cooked_file(&bytes).expect("wrapper");
        let mesh = CookedMesh::from_postcard(payload).expect("mesh");
        mesh.validate().expect("valid");
        assert_eq!(mesh.submeshes.len(), 2, "chunk {i}");
        let cpu = mesh.to_cpu_mesh().expect("unpack");
        assert_eq!(cpu.normals.len(), cpu.positions.len());
        assert_eq!(cpu.tangents.len(), cpu.positions.len());
        // The surface kept the glTF's linear UV mapping through the cuts (within the f16 stream).
        // The cook turned the file to -Z forward, so x and z are negated against the file's.
        let surface = mesh.submeshes[0].index_byte_length as usize
            / match mesh.index_width {
                CookedIndexWidth::U16 => 2,
                CookedIndexWidth::U32 => 4,
            };
        for v in &cpu.indices[..surface] {
            let (p, uv) = (cpu.positions[*v as usize], cpu.uvs[*v as usize]);
            assert!((uv[0] + p[0] + p[2]).abs() < 1e-2, "{uv:?} at {p:?}");
        }
        assert!(c.meta.sub_assets.iter().any(|s| s.id == id && s.path == path));
    }

    // The `.meta` lists the sub-assets and the manifest finds them by name.
    assert!(c.meta.sub_assets.iter().any(|s| {
        s.id == c.destructible
            && s.path == FRACTURE_DESTRUCTIBLE_SUB_PATH
            && s.type_path == DESTRUCTIBLE_TYPE_PATH
    }));
    let manifest = AssetManifest::read_from_cache(&c.cache)
        .expect("manifest")
        .expect("present");
    assert_eq!(
        manifest.id_for_path(&format!("assets/wall.glb#{FRACTURE_DESTRUCTIBLE_SUB_PATH}")),
        Some(c.destructible)
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_unchanged_source_is_not_fractured_again_and_a_changed_setting_is() {
    let root = regia_test_utils::unique_temp_dir("fracture-cook-skip");
    let glb = write_glb(&root, false);
    write_settings(&glb, "(fracture: Some((chunks: 4, seed: 1)))");
    cook(&root).expect("cook");
    let c = cooked(&root, &glb);
    let file = c.cache.join(cooked_file_name(c.destructible));
    let first = std::fs::read(&file).expect("read");
    let written = std::fs::metadata(&file)
        .and_then(|m| m.modified())
        .expect("mtime");

    // Same inputs: the cooked file stays untouched, and the sub-asset rows stay listed.
    cook(&root).expect("cook again");
    assert_eq!(std::fs::read(&file).expect("read"), first);
    assert_eq!(
        std::fs::metadata(&file)
            .and_then(|m| m.modified())
            .expect("mtime"),
        written
    );
    assert!(
        cooked(&root, &glb)
            .meta
            .sub_assets
            .iter()
            .any(|s| s.id == c.destructible)
    );

    // Another seed recooks it.
    write_settings(&glb, "(fracture: Some((chunks: 4, seed: 2)))");
    cook(&root).expect("recook");
    assert_ne!(std::fs::read(&file).expect("read"), first);
    assert_eq!(read_asset(&c).fracture_seed, Some(2));
    // The chunk count is the settings', not the last cook's.
    write_settings(&glb, "(fracture: Some((chunks: 9, seed: 2)))");
    cook(&root).expect("recook");
    assert_eq!(read_asset(&c).chunks.len(), 9);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_renamed_source_names_its_chunks_by_the_new_path() {
    let root = regia_test_utils::unique_temp_dir("fracture-cook-rename");
    let glb = write_glb(&root, false);
    write_settings(&glb, "(fracture: Some((chunks: 4, seed: 1)))");
    cook(&root).expect("cook");

    // The `.meta` moves with the source, so the ids stay and only the path changes.
    let moved = glb.with_file_name("moved.glb");
    std::fs::rename(&glb, &moved).expect("rename source");
    std::fs::rename(meta_path_for(&glb), meta_path_for(&moved)).expect("rename meta");
    cook(&root).expect("cook again");

    let c = cooked(&root, &moved);
    let asset = read_asset(&c);
    assert_eq!(asset.name, "moved");
    assert_eq!(asset.chunks.len(), 4);
    let manifest = AssetManifest::read_from_cache(&c.cache)
        .expect("manifest")
        .expect("present");
    for (i, chunk) in asset.chunks.iter().enumerate() {
        let lod = format!("assets/moved.glb#{}", fracture_chunk_sub_path(i));
        assert_eq!(chunk.lods, vec![lod.clone()]);
        assert!(
            manifest.id_for_path(&lod).is_some(),
            "{lod} is not in the manifest"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn two_cooks_of_the_same_source_give_the_same_bytes() {
    let run = || {
        let root = regia_test_utils::unique_temp_dir("fracture-cook-same");
        let glb = write_glb(&root, false);
        write_settings(&glb, "(fracture: Some((chunks: 5, seed: 8)))");
        cook(&root).expect("cook");
        let c = cooked(&root, &glb);
        let bytes = std::fs::read(c.cache.join(cooked_file_name(c.destructible))).expect("read");
        let _ = std::fs::remove_dir_all(&root);
        bytes
    };
    assert_eq!(run(), run());
}

#[test]
fn a_mesh_that_cannot_be_fractured_fails_its_cook_with_a_code() {
    let root = regia_test_utils::unique_temp_dir("fracture-cook-open");
    let glb = write_glb(&root, true);
    write_settings(&glb, "(fracture: Some((chunks: 4)))");
    let err = cook(&root).expect_err("open mesh");
    assert!(err.contains("fracture_not_closed"), "{err}");
    let _ = std::fs::remove_dir_all(&root);

    let root = regia_test_utils::unique_temp_dir("fracture-cook-bad");
    let glb = write_glb(&root, false);
    write_settings(&glb, "(fracture: Some((chunks: 0)))");
    let err = cook(&root).expect_err("no chunks");
    assert!(err.contains("fracture_invalid_settings"), "{err}");
    write_settings(&glb, "(fracture: Some((mesh: Some(\"mesh/nope\"))))");
    let err = cook(&root).expect_err("no such mesh");
    assert!(err.contains("mesh/nope"), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_obj_takes_the_same_path() {
    let root = regia_test_utils::unique_temp_dir("fracture-cook-obj");
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets");
    let obj = assets.join("cube.obj");
    std::fs::write(
        &obj,
        "v -1 -1 -1\nv 1 -1 -1\nv 1 1 -1\nv -1 1 -1\nv -1 -1 1\nv 1 -1 1\nv 1 1 1\nv -1 1 1\n\
         f 1 3 2\nf 1 4 3\nf 5 6 7\nf 5 7 8\nf 1 2 6\nf 1 6 5\nf 4 8 7\nf 4 7 3\nf 1 5 8\nf 1 8 4\nf 2 3 7\nf 2 7 6\n",
    )
    .expect("obj");
    let meta = "(format_version: 1, id: \"11111111-2222-4333-8444-666666666666\", importer: \"obj\", \
                settings: Some(\"(fracture: Some((chunks: 4, seed: 2)))\"))";
    std::fs::write(meta_path_for(&obj), meta).expect("meta");
    cook(&root).expect("cook");
    let c = cooked(&root, &obj);
    assert_eq!(read_asset(&c).chunks.len(), 4);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn without_the_setting_nothing_is_fractured() {
    let root = regia_test_utils::unique_temp_dir("fracture-cook-off");
    let glb = write_glb(&root, false);
    cook(&root).expect("cook");
    let c = cooked(&root, &glb);
    assert!(c.meta.sub_assets.iter().all(|s| !s.path.starts_with("fracture/")));
    let _ = std::fs::remove_dir_all(&root);
}

/// The Fracture panel's plane support rule (`supports: { below_y }`, ADR-9750) is the `.meta`'s
/// `support_below_y`: written the way **Apply to .meta** writes it, it decides the support flags of
/// every cook, and a re-cook with another seed (new chunks) still follows it.
#[test]
fn fracture_plane_support_rule_in_the_meta_survives_a_recook() {
    use regia_asset_pipeline::fracture_cook::{FractureCookSettings, set_meta_fracture};
    let root = regia_test_utils::unique_temp_dir("fracture-cook-support-plane");
    let glb = write_glb(&root, false);
    // The box spans y -0.5..0.5; the plane holds every chunk reaching below y = -0.2, far more than
    // the default lowest centimetre.
    let mut settings = FractureCookSettings {
        chunks: 12,
        seed: 5,
        support_below_y: Some(-0.2),
        ..FractureCookSettings::default()
    };
    for seed in [5, 6] {
        settings.seed = seed;
        set_meta_fracture(&glb, Some(&settings)).expect("apply to .meta");
        cook(&root).expect("cook");
        let asset = read_asset(&cooked(&root, &glb));
        assert_eq!(asset.fracture_seed, Some(seed));
        let floor = asset
            .chunks
            .iter()
            .map(|c| c.bounds_min[1])
            .fold(f32::INFINITY, f32::min);
        let by_plane: Vec<bool> = asset.chunks.iter().map(|c| c.bounds_min[1] <= -0.2).collect();
        let by_default: Vec<bool> = asset
            .chunks
            .iter()
            .map(|c| c.bounds_min[1] <= floor + 0.01)
            .collect();
        let flags: Vec<bool> = asset.chunks.iter().map(|c| c.support).collect();
        assert_eq!(flags, by_plane, "seed {seed}: the plane decides");
        assert_ne!(flags, by_default, "seed {seed}: not the default rule");
    }
    let _ = std::fs::remove_dir_all(&root);
}
