// SPDX-License-Identifier: MIT-0
//! A shipped game renders its cooked pak like the editor's glTF path (lane cooked-runtime).
//!
//! An own DO27-like project (outpost building glTF with textured walls, tinted double-sided
//! glass, an emissive lamp and a double-sided fin; a terrain; a rock scatter file; a level) goes
//! through `regia cook`. The paks are mounted by the runtime asset server from a directory with
//! no source files and no cooked cache, the outpost is rebuilt from them
//! (`regia_render::cooked_model`) and rendered beside the direct glTF path
//! (`lookdev::hero::load`, RGBA8 source textures):
//!
//! - BC device (cooked BC7 / BC5 chains uploaded as they are) vs direct: within the BC error
//!   bounds below, which come from the fixture textures' measured BC7 / BC5 error;
//! - device without block compression (CPU decode fallback) vs the BC device: the same decode;
//! - per-material double-sided culling: the fin's back face draws only with it;
//! - texture memory: the cooked chains are a quarter of the RGBA8 uploads (BC7 1 B/texel).
//!
//! The `regia` CLI is taken from this test's target dir and profile (`CARGO_BIN_EXE_regia`
//! is only set inside `regia_cli`). When it is not there yet, the test builds it once with
//! `cargo build -p regia_cli --bin regia`. `REGIA_COOKED_RUNTIME_OUT=<dir>` writes the
//! frames and difference images.

use std::path::{Path, PathBuf};

use lookdev::cooked_runtime::{self as cr, View, fixture};
use lookdev::hero::{self, HeroAsset};
use regia_asset::{AssetSource, PakReader, decode_cooked_file};
use regia_core::glam::Vec3;
use regia_render::{LabMaterial, RenderDevice};

const W: u32 = 480;
const H: u32 = 270;

/// Mean / 99.9th-percentile channel-difference bounds of the cooked frame against the direct
/// one. The fixture's textures lose, through the cook (BC7 colour and ORM, BC5 normals, f16 UVs,
/// octahedral normals, MikkTSpace tangents), the error measured by `texture_error` below; the
/// frame bound is that texture error carried through lighting (see the report printed by the
/// test and `docs/evidence/cooked-runtime/README.md`).
const MEAN_BOUND: f64 = 1.5;
const P999_BOUND: u8 = 24;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("regia-cooked-runtime-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn copy_paks(from: &Path, to: &Path) -> Vec<PathBuf> {
    std::fs::create_dir_all(to).expect("content dir");
    cr::pak_files(from)
        .expect("paks")
        .into_iter()
        .map(|pak| {
            let dest = to.join(pak.file_name().expect("pak name"));
            std::fs::copy(&pak, &dest).expect("copy pak");
            dest
        })
        .collect()
}

/// Largest and mean channel error of `decoded` against `source` (RGB only).
fn texture_error(source: &[u8], decoded: &[u8]) -> (u8, f64) {
    let mut max = 0u8;
    let mut sum = 0u64;
    let mut n = 0u64;
    for (a, b) in source.as_chunks::<4>().0.iter().zip(decoded.as_chunks::<4>().0) {
        for c in 0..3 {
            let d = a[c].abs_diff(b[c]);
            max = max.max(d);
            sum += u64::from(d);
            n += 1;
        }
    }
    (max, sum as f64 / n.max(1) as f64)
}

fn render(
    device: &RenderDevice,
    meshes: &[regia_render::LabMesh],
    materials: Vec<LabMaterial>,
    view: View,
) -> cr::Frame {
    cr::render_model(device, meshes, materials, view, W, H).expect("frame")
}

/// A camera 6 m behind `fin` (against its geometric normal, slightly raised) looking at it.
fn fin_back_view(fin: &regia_render::LabMesh) -> View {
    let p = |i: u32| Vec3::from_array(fin.vertices[i as usize].position);
    let tri = &fin.indices[..3];
    let normal = (p(tri[1]) - p(tri[0])).cross(p(tri[2]) - p(tri[0])).normalize();
    let centre = fin
        .vertices
        .iter()
        .map(|v| Vec3::from_array(v.position))
        .sum::<Vec3>()
        / fin.vertices.len() as f32;
    View {
        eye: centre - normal * 6.0 + Vec3::Y * 0.5,
        target: centre,
    }
}

#[test]
#[allow(clippy::too_many_lines)] // one pipeline end to end: cook, pak, mount, load, four renders
fn cooked_pak_renders_like_the_direct_gltf() {
    let cli = cr::regia_cli();
    assert!(
        cli.is_file(),
        "build the cook CLI first: cargo build -p regia_cli --bin regia (missing {})",
        cli.display()
    );
    let root = scratch("project");
    fixture::write_project(
        &root,
        include_str!("../../../crates/regia_templates/templates/blank/assets/levels/main.level.ron"),
    )
    .expect("fixture project");
    // GPU-ready pak meshes are opt-in (ADR-285): this test checks they render like the direct glTF.
    let run = cr::cook_with_env(
        &cli,
        &root,
        "linux",
        &[(regia_asset_pipeline::PAK_GPU_MESHES_ENV, "1")],
    )
    .expect("regia cook");
    eprintln!(
        "cook: {:.1} s, paks {} KiB\n{}",
        run.elapsed.as_secs_f64(),
        run.pak_bytes / 1024,
        run.log
    );

    // The runtime sees only the paks: no `assets/`, no cooked cache.
    let ship = scratch("ship");
    let content = ship.join("content");
    let paks = copy_paks(&run.pak_dir, &content);
    let mut server = cr::mount_paks(&content, &ship.join("runtime"), "linux").expect("server");
    assert!(server.has_paks());
    let gltf = server
        .id_for_path("assets/hero/outpost.gltf")
        .expect("the pak manifest names the outpost");
    let mut cooked = regia_render::cooked_model::load_cooked_model(&mut server, gltf).expect("cooked model");
    assert_eq!(cooked.meshes.len(), 4, "walls, glass, lamp, fin");
    // The opted-in desktop pak stores its meshes GPU-ready (no decode or tangent pass at load).
    let gpu_tag = regia_asset::AssetTypeTag::gpu_mesh().0;
    let gpu_meshes: usize = paks
        .iter()
        .map(|p| {
            let pak = PakReader::open(p).expect("pak");
            pak.toc.entries.iter().filter(|e| e.type_tag == gpu_tag).count()
        })
        .sum();
    assert!(gpu_meshes >= 4, "{gpu_meshes} GPU-ready meshes in the pak");
    assert!(
        matches!(
            server.source_of(regia_render::cooked_model::model_id(gltf)),
            AssetSource::Pak(_)
        ),
        "the model must come from the pak"
    );
    let walls = &cooked.materials[cooked.meshes[0].material];
    let maps = walls
        .compressed
        .as_ref()
        .expect("cooked walls carry block-compressed maps");
    let albedo = maps.albedo.as_ref().expect("albedo chain");
    let normal = maps.normal.as_ref().expect("normal chain");
    let orm = maps.occlusion.as_ref().expect("orm chain");
    assert_eq!(albedo.format, wgpu::TextureFormat::Bc7RgbaUnormSrgb);
    assert_eq!(normal.format, wgpu::TextureFormat::Bc5RgUnorm);
    assert_eq!(orm.format, wgpu::TextureFormat::Bc7RgbaUnorm);
    assert_eq!(albedo.levels.len(), 9, "256² keeps its full mip chain");
    let glass = &cooked.materials[cooked.meshes[1].material];
    assert!(
        glass.double_sided && glass.transmission > 0.9,
        "cooked glass: {glass:?}"
    );
    let fin = &cooked.materials[cooked.meshes[3].material];
    assert!(fin.double_sided);

    // Terrain and scatter come from the same pak. The manifest maps the layer file to its terrain
    // index; the map id the tiles derive from (the source `.meta` id) is read from that index, since
    // a shipped game has no `.meta` sidecars.
    let pak = PakReader::open(&paks[0]).expect("pak");
    let terrain_index = server
        .id_for_path("assets/ground.terrain.layers.ron")
        .expect("terrain in the manifest");
    let index = regia_terrain::stream::load_terrain_index_bytes(
        &pak.read_decompressed(terrain_index).expect("terrain index bytes"),
    )
    .expect("terrain index from the pak");
    assert_eq!(index.world_size_m, Some(64.0));
    assert!(
        index.heightfield.is_some(),
        "the fixture terrain has a heightfield"
    );
    let source = regia_terrain::PakTileSource::new(index.map_id, Some(&content), ship.join("no-cache"));
    assert_eq!(
        source
            .load_terrain_index()
            .expect("index through the tile source")
            .map_id,
        index.map_id
    );
    let heightfield = source.load_heightfield().expect("heightfield from the pak");
    assert_eq!((heightfield.width, heightfield.height), (64, 64));
    let scatter = server
        .id_for_path("assets/rocks.scatter.ron")
        .expect("scatter in the manifest");
    let bytes = pak.read_decompressed(scatter).expect("scatter bytes");
    let (_, payload) = decode_cooked_file(&bytes).expect("cooked scatter");
    regia_terrain::scatter::ScatterSet::from_cooked(payload, None).expect("scatter set from the pak");

    // Measured texture error of the cook (level 0 against the source pixels).
    let s = 256;
    let decoded = |t: &regia_render::CompressedTexture| t.decode_rgba8().expect("decode").rgba;
    let (albedo_max, albedo_mean) = texture_error(&fixture::albedo(s), &decoded(albedo));
    let (orm_max, orm_mean) = texture_error(&fixture::orm(s), &decoded(orm));
    let (normal_max, normal_mean) = texture_error(&fixture::normal(s), &decoded(normal));
    eprintln!(
        "texture error (max / mean channel): albedo BC7 {albedo_max} / {albedo_mean:.2}, \
         ORM BC7 {orm_max} / {orm_mean:.2}, normal BC5 (Z rebuilt) {normal_max} / {normal_mean:.2}"
    );

    // The direct glTF path, in the harness frame; the cooked model moved into it.
    let direct = hero::load(
        &HeroAsset {
            name: "outpost".into(),
            files: vec![root.join("assets/hero/outpost.gltf")],
            planet: lookdev::Planet::Mars,
        },
        4096,
    )
    .expect("direct glTF");
    cr::to_harness_frame(&mut cooked.meshes);
    let (lo, hi) = cr::bounds(&direct.meshes);
    let (clo, chi) = cr::bounds(&cooked.meshes);
    assert!(
        (lo - clo).abs().max_element() < 1e-3 && (hi - chi).abs().max_element() < 1e-3,
        "cooked bounds {clo}..{chi} vs direct {lo}..{hi}"
    );
    let views = [
        ("wide", View::of_bounds(lo, hi, Vec3::new(-0.6, 0.35, 0.75), 0.9)),
        ("close", View::of_bounds(lo, hi, Vec3::new(-0.25, 0.1, 1.0), 0.35)),
        // Facing the fin's back face (the side its geometry culls), 6 m out.
        ("fin_side", fin_back_view(&cooked.meshes[3])),
    ];

    let out = std::env::var_os("REGIA_COOKED_RUNTIME_OUT").map(PathBuf::from);
    for (name, view) in views {
        let bc = RenderDevice::headless().expect("device");
        let has_bc = bc
            .device
            .features()
            .contains(wgpu::Features::TEXTURE_COMPRESSION_BC);
        let a = render(&bc, &direct.meshes, direct.materials.clone(), view);
        let b = render(&bc, &cooked.meshes, cooked.materials.clone(), view);
        let plain = cr::device_without_block_compression().expect("device without BC");
        let c = render(&plain, &cooked.meshes, cooked.materials.clone(), view);
        let mut culled = cooked.materials.clone();
        for m in &mut culled {
            m.double_sided = false;
        }
        let d = render(&bc, &cooked.meshes, culled, view);

        let cooked_vs_direct = cr::diff(&a.pixels, &b.pixels);
        let decode_vs_bc = cr::diff(&b.pixels, &c.pixels);
        let fin_effect = cr::diff(&b.pixels, &d.pixels);
        eprintln!(
            "{name} (device BC: {has_bc}): cooked vs direct {cooked_vs_direct:?}\n  \
             CPU-decode fallback vs BC upload {decode_vs_bc:?}\n  double-sided off vs on {fin_effect:?}"
        );
        if let Some(dir) = &out {
            cr::save(&a, &dir.join(format!("{name}_direct.png"))).expect("png");
            cr::save(&b, &dir.join(format!("{name}_cooked.png"))).expect("png");
            cr::save(&c, &dir.join(format!("{name}_cooked_cpu_decode.png"))).expect("png");
            cr::save(&d, &dir.join(format!("{name}_cooked_single_sided.png"))).expect("png");
            cr::save(
                &cr::diff_image(&a, &b, 8),
                &dir.join(format!("{name}_diff_x8.png")),
            )
            .expect("png");
        }
        assert!(
            cooked_vs_direct.mean <= MEAN_BOUND && cooked_vs_direct.p999 <= P999_BOUND,
            "{name}: cooked frame is outside the BC error bound: {cooked_vs_direct:?}"
        );
        assert!(
            decode_vs_bc.mean <= 0.5 && decode_vs_bc.p999 <= 8,
            "{name}: the CPU decode fallback differs from the GPU's BC decode: {decode_vs_bc:?}"
        );
        if name == "fin_side" {
            assert!(
                fin_effect.over_8 > 200,
                "{name}: the double-sided fin must draw its back face only with per-material culling off: {fin_effect:?}"
            );
        }
    }

    // Texture memory: RGBA8 chains of the direct path vs the cooked block-compressed chains.
    let rgba = cr::rgba8_material_bytes(&direct.materials);
    eprintln!(
        "material textures: direct RGBA8 {} KiB, cooked {} KiB ({:.2}x smaller)",
        rgba / 1024,
        cooked.texture_bytes / 1024,
        rgba as f64 / cooked.texture_bytes.max(1) as f64
    );
    assert!(
        rgba >= 3 * cooked.texture_bytes,
        "cooked textures must be about 4x smaller ({rgba} vs {})",
        cooked.texture_bytes
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&ship);
}
