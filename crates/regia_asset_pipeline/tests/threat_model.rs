// SPDX-License-Identifier: MPL-2.0
//! Importer limits named by `docs/security/threat-model.md` that had no test of their own (ADR-3206).

use regia_asset_pipeline::dds_import::parse_dds;

/// A DDS header claiming `width`×`height`, with no pixels behind it.
fn dds_header(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = b"DDS ".to_vec();
    let mut header = vec![0_u8; 124];
    header[0..4].copy_from_slice(&124_u32.to_le_bytes());
    header[8..12].copy_from_slice(&height.to_le_bytes());
    header[12..16].copy_from_slice(&width.to_le_bytes());
    header[72..76].copy_from_slice(&32_u32.to_le_bytes());
    bytes.extend_from_slice(&header);
    bytes
}

/// The edge check names the size; anything else that stops the file does not.
#[test]
fn a_dds_edge_over_the_limit_is_refused_by_the_size_check() {
    for (width, height) in [(1 << 20, 4), (4, 1 << 20), (u32::MAX, u32::MAX)] {
        let error = parse_dds(&dds_header(width, height))
            .expect_err("too big")
            .to_string();
        assert!(error.contains("is outside"), "{width}x{height}: {error}");
    }
}

/// CRC-32 of a PNG chunk's type and data (ISO 3309), for a header the decoder accepts.
fn png_crc(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Importers call `image::load_from_memory`, whose default limits are all that stands between a small
/// PNG and a canvas of its header's size: 60000×60000 RGBA is 14 GiB. This pins that the default is
/// in force, so an `image` upgrade or a `no_limits` call that drops it fails here.
#[test]
fn a_png_claiming_a_huge_canvas_is_refused_by_the_decoder_limit() {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = b"IHDR".to_vec();
    ihdr.extend_from_slice(&60_000_u32.to_be_bytes());
    ihdr.extend_from_slice(&60_000_u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    png.extend_from_slice(&13_u32.to_be_bytes());
    png.extend_from_slice(&ihdr);
    png.extend_from_slice(&png_crc(&ihdr).to_be_bytes());
    // The decoder reads up to the first data chunk before it looks at the size.
    for name in [&b"IDAT"[..], &b"IEND"[..]] {
        png.extend_from_slice(&0_u32.to_be_bytes());
        png.extend_from_slice(name);
        png.extend_from_slice(&png_crc(name).to_be_bytes());
    }
    let error = image::load_from_memory(&png).expect_err("too big").to_string();
    assert!(
        error.to_ascii_lowercase().contains("limit") || error.to_ascii_lowercase().contains("memory"),
        "{error}"
    );
}
