// SPDX-License-Identifier: MPL-2.0
//! Allocation bounds for hot asset paths added since the v040 audit (performance lane).
//!
//! The tagging allocator counts every heap byte and allocation in this binary. Each bound is the
//! buffers the call has to keep (the decoded atlas, the cooked mesh file, the matched-rule list),
//! plus a small slack for the header and the geometry of one entry. A full-size temporary — a
//! float image beside the RGB9E5 texels, a second and third copy of a direction atlas, another
//! copy of the vertex buffer, a dynamic-programming table per glob — blows the bound.
//!
//! The ledger is the process's: it also counts what the block encoder's scoped threads allocate,
//! and what any other thread does meanwhile. So every test here runs alone from its first line
//! (`alloc_counter::exclusive`, ADR-5718). A lock around the measured call alone let another
//! test's input (a 196 KB float atlas, a 786 KB vertex buffer) land in the window: with 4 test
//! threads the direction and mesh tests failed 20 runs of 20, with one thread none.

regia_test_utils::install_tagging_allocator!();

use regia_asset::import_rules::ImportRulesFile;
use regia_asset::lightmap::{
    Coverage, Lightmap, LightmapBakeInfo, LightmapEntry, LightmapLayout, LightmapTexelEncoding, MeshUnwrap,
};
use regia_asset::{AssetId, GpuMeshInfo, GpuMeshLod, encode_gpu_mesh_file};
use regia_core::memory::{MemorySnapshot, snapshot};

fn totals(snapshot: &MemorySnapshot) -> (u64, u64) {
    snapshot
        .subsystems
        .iter()
        .fold((0, 0), |(bytes, allocs), (_, memory)| {
            (bytes + memory.total_bytes, allocs + memory.total_allocations)
        })
}

/// Bytes and allocations made by `body`, on every thread. The calling test holds
/// `alloc_counter::exclusive`, so no other test of this binary runs beside it.
fn measured(body: impl FnOnce()) -> (u64, u64) {
    let before = snapshot();
    assert!(before.tracking, "the tagging allocator is not installed");
    body();
    let after = snapshot();
    let (before_bytes, before_allocs) = totals(&before);
    let (after_bytes, after_allocs) = totals(&after);
    (
        after_bytes.saturating_sub(before_bytes),
        after_allocs.saturating_sub(before_allocs),
    )
}

fn quad_entry(width: u32, height: u32) -> (LightmapEntry, MeshUnwrap) {
    (
        LightmapEntry {
            name: "quad".to_string(),
            source_vertices: 4,
            rect: [0, 0, width, height],
            scale_offset: [1.0, 1.0, 0.0, 0.0],
            vertex_count: 4,
            index_count: 6,
            first_vertex: 0,
            first_index: 0,
            charts: 1,
            area_m2: 1.0,
            texels_per_metre: 1.0,
            key: None,
            seams: None,
            page: 0,
        },
        MeshUnwrap {
            xref: vec![0, 1, 2, 3],
            uv: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            indices: vec![0, 1, 2, 0, 2, 3],
        },
    )
}

/// A one-page atlas of smooth light (a block's texels stay within a factor of two).
fn smooth_atlas(width: u32, height: u32) -> Lightmap {
    let (entry, unwrap) = quad_entry(width, height);
    let n = width as usize * height as usize;
    let irradiance: Vec<[f32; 3]> = (0..n)
        .map(|i| {
            let x = (i % width as usize) as f32;
            let y = (i / width as usize) as f32;
            [100.0 + x * 0.05, 80.0 + y * 0.05, 60.0]
        })
        .collect();
    Lightmap::from_irradiance(
        LightmapLayout {
            width,
            height,
            density: 1.0,
            padding: 1,
            entries: vec![entry],
            unwraps: vec![unwrap],
        },
        &irradiance,
        vec![Coverage::BAKED; n],
        LightmapBakeInfo::default(),
    )
    .expect("smooth atlas")
}

#[test]
fn block_lightmap_load_does_not_allocate_a_float_image() {
    let _alone = regia_test_utils::alloc_counter::exclusive();
    // 128²: the float image the loader used to keep is 128² × 12 = 196_608 bytes, past the slack.
    const W: u32 = 128;
    const H: u32 = 128;
    let mut map = smooth_atlas(W, H);
    let (encode_bytes, _) = measured(|| {
        map.encode_texels(LightmapTexelEncoding::Bc6h).expect("encode");
    });
    let n = (W * H) as u64;
    let block_bytes = map.blocks.as_ref().expect("blocks").data.len() as u64;
    // Source floats (the encoder's input), the mask, the relative-error samples (3 per texel),
    // the block bytes and the RGB9E5 texels the CPU readers keep. Not a second float image.
    let encode_bound = n * 12 + n + n * 12 + block_bytes + n * 4 + 64 * 1024;
    assert!(
        encode_bytes <= encode_bound,
        "encode allocated {encode_bytes} bytes, bound {encode_bound} (a float image is {})",
        n * 12
    );

    let bytes = map.to_bytes().expect("write");
    let original = map.texels.clone();
    let (load_bytes, _) = measured(|| {
        let loaded = Lightmap::from_bytes(&bytes).expect("read");
        assert_eq!(loaded.texels, original, "the CPU decode is unchanged");
        assert_eq!(
            loaded.blocks.as_ref().map(|b| b.data.len()),
            Some(block_bytes as usize)
        );
    });
    // RGB9E5 texels, coverage, the stored blocks, and the postcard header. Not the float image.
    let load_bound = n * 4 + n + block_bytes + 32 * 1024;
    assert!(
        load_bytes <= load_bound,
        "load allocated {load_bytes} bytes, bound {load_bound} (a float image is {})",
        n * 12
    );
}

#[test]
fn directional_lightmap_load_keeps_one_direction_buffer() {
    let _alone = regia_test_utils::alloc_counter::exclusive();
    const W: u32 = 128;
    const H: u32 = 128;
    let mut map = smooth_atlas(W, H);
    let n = (W * H) as usize;
    let directions = vec![[0.0, 1.0, 0.0, 1.0]; n];
    map.set_directions(&directions).expect("directions");
    let bytes = map.to_bytes().expect("write");
    let original = map.directions.clone();
    let (load_bytes, _) = measured(|| {
        let loaded = Lightmap::from_bytes(&bytes).expect("read");
        assert_eq!(loaded.directions, original, "direction texels are unchanged");
    });
    let n = n as u64;
    // Irradiance, coverage, one direction buffer. Not the byte copy plus a second u32 copy.
    let bound = n * 4 + n + n * 4 + 32 * 1024;
    assert!(
        load_bytes <= bound,
        "load allocated {load_bytes} bytes, bound {bound} (one extra direction buffer is {})",
        n * 4
    );
}

#[test]
fn gpu_mesh_file_is_encoded_once() {
    let _alone = regia_test_utils::alloc_counter::exclusive();
    const VERTICES: usize = 8_192;
    let vertices = vec![0u8; VERTICES * regia_asset::GPU_VERTEX_STRIDE as usize];
    let indices = vec![0u32, 1, 2];
    let info = GpuMeshInfo {
        aabb_min: [0.0; 3],
        aabb_max: [1.0, 0.0, 0.0],
        bounding_sphere: [0.0, 0.0, 0.0, 1.0],
        lods: vec![GpuMeshLod {
            screen_relative_size: 1.0,
            first_index: 0,
            index_count: 3,
        }],
    };
    let id = AssetId::new_random();
    let (used, _) = measured(|| {
        let file = encode_gpu_mesh_file(id, 1, Vec::new(), &info, &vertices, &indices).expect("encode");
        assert!(file.len() > vertices.len(), "the file holds the vertices");
    });
    let vertex_bytes = vertices.len() as u64;
    // The payload once, then the cooked file that copies it. Not a second payload while the
    // header length settles.
    let bound = vertex_bytes * 2 + 96 * 1024;
    assert!(
        used <= bound,
        "encode allocated {used} bytes for {vertex_bytes} vertex bytes, bound {bound}"
    );
}

#[test]
fn import_and_path_globs_do_not_allocate_per_match() {
    let _alone = regia_test_utils::alloc_counter::exclusive();
    let mut text = String::from("(format_version: 1, rules: [");
    for _ in 0..32 {
        text.push_str("(glob: \"assets/**\", settings: \"()\"),");
    }
    text.push_str("])");
    let file = ImportRulesFile::from_ron(&text).expect("rules");
    let (bytes, allocs) = measured(|| {
        for _ in 0..40 {
            let matched = file.matched_rules("assets/props/Crate.PNG", "png");
            assert_eq!(matched.len(), 32);
            assert!(regia_asset::registry::path_glob_match(
                "assets/**/*.png",
                "assets/props/crate.png"
            ));
            assert!(regia_asset::import_rules::glob_match(
                "assets/**/*.png",
                "assets/props/crate.png"
            ));
        }
    });
    // One result list per call, grown a handful of times. Not a table per rule.
    assert!(
        allocs <= 800,
        "40 matches of 32 rules allocated {allocs} times ({bytes} bytes)"
    );
}
