// SPDX-License-Identifier: MPL-2.0
//! Load time of one textured glTF three ways (ADR-1594, DO27 request DO-3), CPU side only (no GPU
//! upload):
//!
//! 1. `runtime_gltf`: what a game that parses its sources at load does at the least: `gltf::import`
//!    (JSON, buffers, PNG decode to RGBA8), the accessors read into vectors and MikkTSpace tangents
//!    generated. No mips, no block compression: a lower bound of that path.
//! 2. `compact_pak`: `regia cook` as before: the pak's compact mesh is unpacked and gets tangents,
//!    the BC textures are views of the pak.
//! 3. `cooked_pack`: `regia cook --ship`: the GPU-ready mesh is copied as is, the BC textures are
//!    views of the pak.
//!
//! Each load opens a fresh `AssetServer` and calls `load_cooked_model`; the OS page cache is warm
//! after the first run, which is the steady state of a game's second launch. Run with
//! `REGIA_PAK_VERIFY=0` so the first-launch pak check does not gate zero-copy reads:
//!
//! ```sh
//! REGIA_PAK_VERIFY=0 cargo run --release -p regia_asset_pipeline --example cooked_pack_bench -- \
//!     [--grid N] [--texture N] [--runs N] [--raw]
//! ```
//!
//! `--grid N` is the quad grid's side (N × N quads, default 512 = 524,288 triangles), `--texture N`
//! the side of the two PNGs (default 2048), `--raw` writes uncompressed paks. One JSON line per
//! path: median and min milliseconds over `--runs` loads, plus the pak's size.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use image::ImageEncoder;
use regia_asset::{AssetServer, AssetServerSettings};
use regia_asset_pipeline::{CookOptions, cook_project, host_platform, pack_cooked_directory_with};

type Error = Box<dyn std::error::Error>;

struct Args {
    grid: u32,
    texture: u32,
    runs: usize,
    raw: bool,
}

fn parse_args() -> Result<Args, Error> {
    let mut args = Args {
        grid: 512,
        texture: 2048,
        runs: 5,
        raw: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--grid" => args.grid = value()?.parse()?,
            "--texture" => args.texture = value()?.parse()?,
            "--runs" => args.runs = value()?.parse::<usize>()?.max(1),
            "--raw" => args.raw = true,
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    if args.grid == 0 || args.grid > 2048 || args.texture < 4 || args.texture > 8192 {
        return Err("--grid must be 1..=2048 and --texture 4..=8192".into());
    }
    Ok(args)
}

/// A PNG of `side` × `side` with a pattern that does not compress to nothing.
fn png(side: u32, seed: u32) -> Result<Vec<u8>, Error> {
    let mut pixels = Vec::with_capacity((side * side * 4) as usize);
    for y in 0..side {
        for x in 0..side {
            let h =
                (x.wrapping_mul(73_856_093) ^ y.wrapping_mul(19_349_663) ^ seed).wrapping_mul(2_654_435_761);
            pixels.extend_from_slice(&[(h >> 24) as u8, (x & 0xff) as u8, (y & 0xff) as u8, 255]);
        }
    }
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out).write_image(
        &pixels,
        side,
        side,
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(out)
}

/// `assets/grid.gltf` (+ `grid.bin`, `grid_albedo.png`, `grid_normal.png`): an N × N quad grid
/// with a gentle height wave, normals, UVs and `u32` indices.
fn write_project(root: &Path, grid: u32, texture: u32) -> Result<PathBuf, Error> {
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets)?;
    let side = grid + 1;
    let n = (side * side) as usize;
    let (mut pos, mut nrm, mut uv) = (
        Vec::with_capacity(n * 12),
        Vec::with_capacity(n * 12),
        Vec::with_capacity(n * 8),
    );
    let (mut min, mut max) = ([f32::MAX; 3], [f32::MIN; 3]);
    for z in 0..side {
        for x in 0..side {
            let (u, v) = (x as f32 / grid as f32, z as f32 / grid as f32);
            let p = [
                u * 10.0 - 5.0,
                (u * 12.0).sin() * (v * 9.0).cos() * 0.2,
                v * 10.0 - 5.0,
            ];
            for ((lo, hi), c) in min.iter_mut().zip(max.iter_mut()).zip(p) {
                *lo = lo.min(c);
                *hi = hi.max(c);
            }
            p.iter().for_each(|c| pos.extend_from_slice(&c.to_le_bytes()));
            [0.0f32, 1.0, 0.0]
                .iter()
                .for_each(|c| nrm.extend_from_slice(&c.to_le_bytes()));
            [u, v].iter().for_each(|c| uv.extend_from_slice(&c.to_le_bytes()));
        }
    }
    let mut idx = Vec::with_capacity((grid * grid * 6 * 4) as usize);
    for z in 0..grid {
        for x in 0..grid {
            let i = z * side + x;
            for k in [i, i + side, i + 1, i + 1, i + side, i + side + 1] {
                idx.extend_from_slice(&k.to_le_bytes());
            }
        }
    }
    let (pl, nl, ul, il) = (pos.len(), nrm.len(), uv.len(), idx.len());
    let mut bin = pos;
    bin.extend_from_slice(&nrm);
    bin.extend_from_slice(&uv);
    bin.extend_from_slice(&idx);
    let json = format!(
        r#"{{
  "asset": {{"version": "2.0"}},
  "scene": 0,
  "scenes": [{{"nodes": [0]}}],
  "nodes": [{{"mesh": 0, "name": "Grid"}}],
  "meshes": [{{"primitives": [{{"attributes": {{"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2}}, "indices": 3, "material": 0, "mode": 4}}]}}],
  "materials": [{{"name": "grid", "pbrMetallicRoughness": {{"baseColorTexture": {{"index": 0}}}}, "normalTexture": {{"index": 1}}}}],
  "textures": [{{"source": 0}}, {{"source": 1}}],
  "images": [{{"uri": "grid_albedo.png"}}, {{"uri": "grid_normal.png"}}],
  "buffers": [{{"byteLength": {total}, "uri": "grid.bin"}}],
  "bufferViews": [
    {{"buffer": 0, "byteOffset": 0, "byteLength": {pl}, "target": 34962}},
    {{"buffer": 0, "byteOffset": {o1}, "byteLength": {nl}, "target": 34962}},
    {{"buffer": 0, "byteOffset": {o2}, "byteLength": {ul}, "target": 34962}},
    {{"buffer": 0, "byteOffset": {o3}, "byteLength": {il}, "target": 34963}}
  ],
  "accessors": [
    {{"bufferView": 0, "componentType": 5126, "count": {n}, "type": "VEC3", "min": [{min0}, {min1}, {min2}], "max": [{max0}, {max1}, {max2}]}},
    {{"bufferView": 1, "componentType": 5126, "count": {n}, "type": "VEC3"}},
    {{"bufferView": 2, "componentType": 5126, "count": {n}, "type": "VEC2"}},
    {{"bufferView": 3, "componentType": 5125, "count": {ic}, "type": "SCALAR"}}
  ]
}}"#,
        total = bin.len(),
        o1 = pl,
        o2 = pl + nl,
        o3 = pl + nl + ul,
        ic = il / 4,
        min0 = min[0],
        min1 = min[1],
        min2 = min[2],
        max0 = max[0],
        max1 = max[1],
        max2 = max[2],
    );
    std::fs::write(assets.join("grid.gltf"), json)?;
    std::fs::write(assets.join("grid.bin"), bin)?;
    std::fs::write(assets.join("grid_albedo.png"), png(texture, 1)?)?;
    std::fs::write(assets.join("grid_normal.png"), png(texture, 2)?)?;
    Ok(assets.join("grid.gltf"))
}

/// Path 1: parse the glTF and decode its PNGs at load, then build tangents. Returns the vertex
/// count and the RGBA8 bytes decoded (so nothing is optimised away).
fn runtime_gltf(path: &Path) -> Result<(usize, usize), Error> {
    let (document, buffers, images) = gltf::import(path)?;
    let mut vertices = 0;
    for mesh in document.meshes() {
        for primitive in mesh.primitives() {
            let reader = primitive.reader(|b| buffers.get(b.index()).map(|d| &d.0[..]));
            let positions: Vec<[f32; 3]> = reader.read_positions().ok_or("no positions")?.collect();
            let normals: Vec<[f32; 3]> = reader.read_normals().ok_or("no normals")?.collect();
            let uvs: Vec<[f32; 2]> = reader.read_tex_coords(0).ok_or("no uvs")?.into_f32().collect();
            let indices: Vec<u32> = reader.read_indices().ok_or("no indices")?.into_u32().collect();
            let mut tangents = vec![[1.0, 0.0, 0.0, 1.0]; positions.len()];
            regia_render::generate_vertex_tangents(&positions, &normals, &uvs, &indices, &mut tangents);
            vertices += positions.len();
        }
    }
    Ok((vertices, images.iter().map(|i| i.pixels.len()).sum()))
}

/// Paths 2 and 3: a fresh server on `content` (no cooked cache) loads the model.
fn pak_load(
    content: &Path,
    empty_root: &Path,
    platform: &str,
) -> Result<regia_asset::cooked_pack::LoadForms, Error> {
    let mut server = AssetServer::open(AssetServerSettings {
        project_root: empty_root.to_path_buf(),
        platform: platform.to_owned(),
        content_dir: Some(content.to_path_buf()),
    })?;
    let id = server
        .id_for_path("assets/grid.gltf")
        .ok_or("the pak has no assets/grid.gltf")?;
    let draws = regia_render::cooked_model::load_cooked_model(&mut server, id)?;
    if draws.meshes.is_empty() {
        return Err("no mesh loaded".into());
    }
    Ok(server.load_forms())
}

/// Median and min of `runs` timings of `f`, in milliseconds.
fn time<T>(runs: usize, mut f: impl FnMut() -> Result<T, Error>) -> Result<(f64, f64, T), Error> {
    let mut ms = Vec::with_capacity(runs);
    let mut last = None;
    for _ in 0..runs {
        let start = Instant::now();
        last = Some(f()?);
        ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    ms.sort_by(f64::total_cmp);
    let last = last.ok_or("no run")?;
    Ok((ms[ms.len() / 2], ms[0], last))
}

fn pak_bytes(dir: &Path) -> u64 {
    std::fs::metadata(dir.join("base.fpak")).map_or(0, |m| m.len())
}

fn main() -> Result<(), Error> {
    let args = parse_args()?;
    let platform = host_platform().to_string();
    let root = std::env::temp_dir().join(format!("regia_cooked_pack_bench_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let gltf_path = write_project(&root, args.grid, args.texture)?;
    let empty = root.join("empty");
    std::fs::create_dir_all(&empty)?;

    let started = Instant::now();
    let report = cook_project(CookOptions::for_host(root.clone()))?;
    if !report.errors.is_empty() {
        return Err(format!("cook errors: {:?}", report.errors).into());
    }
    let cook_ms = started.elapsed().as_secs_f64() * 1000.0;
    let cache = regia_asset::cooked_cache_dir(&root, &platform);
    let (compact, gpu) = (root.join("paks_compact"), root.join("paks_gpu"));
    pack_cooked_directory_with(&cache, &compact, &BTreeMap::new(), !args.raw, false)?;
    pack_cooked_directory_with(&cache, &gpu, &BTreeMap::new(), !args.raw, true)?;

    let source_bytes: u64 = std::fs::read_dir(root.join("assets"))?
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum();
    let triangles = u64::from(args.grid) * u64::from(args.grid) * 2;
    println!(
        "{{\"setup\":true,\"platform\":\"{platform}\",\"grid\":{},\"triangles\":{triangles},\"texture\":{},\"runs\":{},\"compressed_paks\":{},\"cook_ms\":{cook_ms:.0},\"source_bytes\":{source_bytes}}}",
        args.grid, args.texture, args.runs, !args.raw
    );

    let (median, min, (vertices, rgba)) = time(args.runs, || runtime_gltf(&gltf_path))?;
    println!(
        "{{\"path\":\"runtime_gltf\",\"median_ms\":{median:.2},\"min_ms\":{min:.2},\"vertices\":{vertices},\"rgba8_bytes\":{rgba}}}"
    );
    for (name, dir) in [("compact_pak", &compact), ("cooked_pack", &gpu)] {
        let (median, min, forms) = time(args.runs, || pak_load(dir, &empty, &platform))?;
        println!(
            "{{\"path\":\"{name}\",\"median_ms\":{median:.2},\"min_ms\":{min:.2},\"pak_bytes\":{},\"gpu_meshes\":{},\"compact_meshes\":{},\"textures\":{},\"cache_loads\":{}}}",
            pak_bytes(dir),
            forms.gpu_meshes,
            forms.compact_meshes,
            forms.textures,
            forms.cache_loads
        );
    }
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}
