// SPDX-License-Identifier: MPL-2.0
//! audit-v035-cook H1: a `.tmj` tile layer's base64 zlib/gzip data was inflated into an unbounded
//! buffer before its length was checked against the map size, so a small file whose layer is the
//! gzip of gigabytes of zeros exhausted memory during a cook. This binary records the largest
//! single allocation request while such a map is imported. Its own test binary: it installs a
//! global allocator.

use std::io::Write;

use base64::Engine;
use regia_asset_pipeline::tiled_import::import_tiled;

use regia_test_utils::alloc_counter::{self, CountingAlloc};

// The shared counting allocator (ADR-3291): the largest single request of any thread.
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

/// A 2 x 2 map whose single tile layer is `inflated` zero bytes, compressed with `codec`.
fn map_json(codec: &str, inflated: usize) -> String {
    let zeros = vec![0u8; 1 << 20];
    let mut left = inflated;
    let compressed = match codec {
        "gzip" => {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
            while left > 0 {
                let n = left.min(zeros.len());
                enc.write_all(&zeros[..n]).expect("gzip in memory");
                left -= n;
            }
            enc.finish().expect("gzip in memory")
        }
        _ => {
            let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
            while left > 0 {
                let n = left.min(zeros.len());
                enc.write_all(&zeros[..n]).expect("zlib in memory");
                left -= n;
            }
            enc.finish().expect("zlib in memory")
        }
    };
    let data = base64::engine::general_purpose::STANDARD.encode(compressed);
    format!(
        r#"{{"width": 2, "height": 2, "tilewidth": 8, "tileheight": 8, "orientation": "orthogonal",
            "layers": [{{"type": "tilelayer", "name": "ground", "width": 2, "height": 2,
                         "encoding": "base64", "compression": "{codec}", "data": "{data}"}}]}}"#
    )
}

#[test]
fn a_compressed_tile_layer_does_not_inflate_past_the_map_size() {
    let dir = regia_test_utils::unique_temp_dir("tiled_inflate_alloc");
    for codec in ["gzip", "zlib"] {
        // 256 MiB of zeros: a few hundred KiB once compressed, far under the 32 MiB file cap.
        let path = dir.join(format!("bomb-{codec}.tmj"));
        std::fs::write(&path, map_json(codec, 256 << 20)).expect("write the map");
        alloc_counter::reset_largest_request();
        let result = import_tiled(&path);
        let largest = alloc_counter::largest_request();
        let err = result.expect_err("a layer that inflates past 2 x 2 tiles is an error");
        assert!(err.to_string().contains("inflates past"), "{codec}: {err}");
        assert!(
            largest < 16 << 20,
            "{codec}: importing the map asked for a {largest}-byte allocation"
        );
        // The same layer with exactly 2 x 2 tiles still imports.
        let good = dir.join(format!("good-{codec}.tmj"));
        std::fs::write(&good, map_json(codec, 16)).expect("write the map");
        let map = import_tiled(&good).unwrap_or_else(|e| panic!("{codec}: {e}")).map;
        assert_eq!((map.width, map.height), (2, 2));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A guard, not a red test: the tile map's own validation already refused such a map, but only
/// after every layer was decoded; the importer now refuses it before reading any layer data.
#[test]
fn a_map_claiming_too_many_tiles_is_refused() {
    let dir = regia_test_utils::unique_temp_dir("tiled_inflate_dims");
    let path = dir.join("huge.tmj");
    std::fs::write(
        &path,
        r#"{"width": 100000, "height": 100000, "tilewidth": 8, "tileheight": 8, "layers": []}"#,
    )
    .expect("write the map");
    import_tiled(&path).expect_err("a 10^10-tile map is refused");
    let _ = std::fs::remove_dir_all(&dir);
}
