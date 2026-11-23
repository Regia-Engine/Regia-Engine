// SPDX-License-Identifier: MPL-2.0
//! Test-gap sweep of 2026-11-04, batch 2 (lane sprint-testgaps-render-pt): lightmap block
//! encodings and their upload rule, video plane conversion guards, the LTC and hair helpers, the
//! thin-film map packer and the permutation enforcement names. Pure CPU, fixed inputs, no device,
//! no goldens. Each test's doc says what bug it would catch.

use regia_asset::baked_gi::from_rgb9e5;
use regia_asset::hdr_blocks::hdr_block_bytes;
use regia_asset::lightmap::{
    Coverage, Lightmap, LightmapBakeInfo, LightmapContents, LightmapEntry, LightmapError, LightmapLayout,
    LightmapTexelEncoding, MeshUnwrap,
};
use regia_core::math::{Mat3, Vec3};
use regia_render::lightmap::{InstalledLightmap, LightmapSettings, lightmap_native_blocks};
use regia_render::ltc::{LTC_SIZE, edge_vector, ltc_texture_bytes, minv_from_entry};
use regia_render::video::{convert_rows, yuv_to_rgba8};
use regia_render::{
    HairFactors, LabTexture, PermutationEnforcement, YuvMatrix, YuvPlanes, pack_thin_film_map,
};

/// A `w × h` lightmap with one entry covering it and a smooth ramp of texels (block formats
/// fit two endpoints per 4 × 4 block: a steep 1/r gradient across three color directions loses
/// up to 86 % in one texel by design, which is not what these tests measure).
fn map(w: u32, h: u32) -> Lightmap {
    let irr: Vec<[f32; 3]> = (0..h)
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .map(|(x, y)| {
            // Smooth, like baked light: a few percent per texel in each channel.
            let (x, y) = (x as f32, y as f32);
            [40.0 + 2.0 * x + y, 30.0 + x + 2.0 * y, 20.0 + 0.5 * x + 0.5 * y]
        })
        .collect();
    Lightmap::from_irradiance(
        LightmapLayout {
            width: w,
            height: h,
            density: 1.0,
            padding: 1,
            entries: vec![LightmapEntry {
                name: "e".into(),
                source_vertices: 3,
                rect: [0, 0, w, h],
                scale_offset: [1.0, 1.0, 0.0, 0.0],
                vertex_count: 0,
                index_count: 0,
                first_vertex: 0,
                first_index: 0,
                charts: 1,
                area_m2: 1.0,
                texels_per_metre: 1.0,
                key: None,
                seams: None,
                page: 0,
            }],
            unwraps: vec![MeshUnwrap {
                xref: vec![0, 1, 2],
                uv: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
                indices: vec![0, 1, 2],
            }],
        },
        &irr,
        vec![Coverage::BAKED; (w * h) as usize],
        LightmapBakeInfo {
            contents: LightmapContents::SKY,
            ..LightmapBakeInfo::default()
        },
    )
    .expect("valid lightmap")
}

fn astc_hdr() -> wgpu::TextureFormat {
    wgpu::TextureFormat::Astc {
        block: wgpu::AstcBlock::B4x4,
        channel: wgpu::AstcChannel::Hdr,
    }
}

// ---------------------------------------------------------------------------------------------
// Lightmap block encodings (ADR-2314)
// ---------------------------------------------------------------------------------------------

/// Catches an upload rule that hands wgpu a compressed level 0 that is not whole blocks (a
/// validation error on a 5 × 3 atlas), uploads blocks the device cannot sample, or uploads the
/// blocks of an RGB9E5 file.
#[test]
fn native_blocks_need_the_feature_and_whole_blocks() {
    let all = wgpu::Features::TEXTURE_COMPRESSION_BC | wgpu::Features::TEXTURE_COMPRESSION_ASTC_HDR;
    let plain = map(8, 4);
    assert!(
        lightmap_native_blocks(&plain, all).is_none(),
        "RGB9E5 has no blocks"
    );

    for (encoding, format, feature) in [
        (
            LightmapTexelEncoding::Bc6h,
            wgpu::TextureFormat::Bc6hRgbUfloat,
            wgpu::Features::TEXTURE_COMPRESSION_BC,
        ),
        (
            LightmapTexelEncoding::AstcHdr,
            astc_hdr(),
            wgpu::Features::TEXTURE_COMPRESSION_ASTC_HDR,
        ),
    ] {
        let mut whole = map(8, 4);
        whole.encode_texels(encoding).expect("encodes");
        let (got, blocks) = lightmap_native_blocks(&whole, feature).expect("whole blocks upload");
        assert_eq!(got, format);
        assert_eq!(blocks.len(), hdr_block_bytes(8, 4));
        assert!(
            lightmap_native_blocks(&whole, wgpu::Features::empty()).is_none(),
            "{encoding:?}"
        );

        let mut ragged = map(5, 3);
        ragged.encode_texels(encoding).expect("encodes");
        assert!(
            lightmap_native_blocks(&ragged, all).is_none(),
            "{encoding:?}: 5 x 3 is not whole blocks"
        );
    }
    // ASTC HDR blocks on a device that only samples BC: decoded RGB9E5 instead.
    let mut astc = map(8, 8);
    astc.encode_texels(LightmapTexelEncoding::AstcHdr)
        .expect("encodes");
    assert!(lightmap_native_blocks(&astc, wgpu::Features::TEXTURE_COMPRESSION_BC).is_none());
}

/// Catches a block encoder that mishandles a ragged edge (texels past the last full block row
/// or column read garbage) or a file round trip that drops the blocks or changes their
/// encoding: the decoded texels stay close to the source and survive `to_bytes`/`from_bytes`.
#[test]
fn ragged_block_encodings_round_trip_through_the_file() {
    for encoding in [LightmapTexelEncoding::Bc6h, LightmapTexelEncoding::AstcHdr] {
        for (w, h) in [(1u32, 1u32), (5, 3), (7, 9)] {
            let source = map(w, h);
            let mut encoded = source.clone();
            let report = encoded.encode_texels(encoding).expect("encodes");
            assert_eq!(report.bytes, hdr_block_bytes(w, h), "{encoding:?} {w}x{h}");
            assert_eq!(report.texels, (w * h) as usize);
            assert!(
                report.max_rel.is_finite() && report.max_rel < 0.05,
                "{encoding:?} {w}x{h}: {report:?}"
            );
            for (a, b) in source.texels.iter().zip(&encoded.texels) {
                let (a, b) = (from_rgb9e5(*a), from_rgb9e5(*b));
                let top = a.iter().copied().fold(0.0f32, f32::max);
                for c in 0..3 {
                    assert!(
                        (a[c] - b[c]).abs() <= 0.05 * top + 1e-3,
                        "{encoding:?} {w}x{h}: {a:?} vs {b:?}"
                    );
                }
            }
            let bytes = encoded.to_bytes().expect("writes");
            let read = Lightmap::from_bytes(&bytes).expect("reads back");
            assert_eq!(read.texel_encoding(), encoding);
            assert_eq!(read.blocks, encoded.blocks);
            assert_eq!(read.texels, encoded.texels);
        }
    }
}

/// Catches an RGB9E5 re-encode that keeps stale blocks (the file would keep saying `bc6h`
/// while the texels changed) or reports the wrong byte count.
#[test]
fn re_encoding_as_rgb9e5_drops_the_blocks() {
    let mut m = map(8, 8);
    m.encode_texels(LightmapTexelEncoding::Bc6h).expect("encodes");
    assert_eq!(m.texel_encoding(), LightmapTexelEncoding::Bc6h);
    let texels = m.texels.clone();
    let report = m.encode_texels(LightmapTexelEncoding::Rgb9e5).expect("drops");
    assert!(m.blocks.is_none());
    assert_eq!(m.texel_encoding(), LightmapTexelEncoding::Rgb9e5);
    assert_eq!(m.texels, texels, "RGB9E5 keeps the texels as they are");
    assert_eq!(report.bytes, 8 * 8 * 4);
    assert_eq!(report.rgb9e5_bytes, report.bytes);
}

/// Catches a status that reports the wrong texel encoding or GPU size for a block-encoded
/// lightmap, and an install error that drops the lightmap already installed.
#[test]
fn installed_status_names_the_encoding_and_a_failed_load_keeps_the_map() {
    let mut installed = InstalledLightmap::default();
    let settings = LightmapSettings::default();
    assert_eq!(installed.status(&settings).texel_encoding, "");
    let mut m = map(8, 4);
    m.encode_texels(LightmapTexelEncoding::AstcHdr).expect("encodes");
    installed.install("host", m);
    let status = installed.status(&settings);
    assert_eq!(status.texel_encoding, "astc_hdr");
    assert_eq!((status.width, status.height, status.entries), (8, 4, 1));
    assert_eq!(status.gpu_bytes, 8 * 4 * 4);
    assert!(status.error.is_none());

    let err = installed.install_result("broken.flmap", Lightmap::from_bytes(b"not a lightmap"));
    let code = match &err {
        Err(e) => e.code(),
        Ok(()) => panic!("garbage must not install"),
    };
    let status = installed.status(&settings);
    assert!(status.installed, "the earlier lightmap stays");
    assert_eq!(status.source.as_deref(), Some("host"));
    assert_eq!(status.error_code.as_deref(), Some(code));
    assert!(code.starts_with("lightmap_"), "{code}");
    installed.clear();
    let status = installed.status(&settings);
    assert!(!status.installed && status.error.is_none() && status.texel_encoding.is_empty());
    // An io error has its own code.
    let missing = std::env::temp_dir().join(format!(
        "regia-tq-render-1104-missing-{}.flmap",
        std::process::id()
    ));
    let e = installed.load(&missing, "missing").expect_err("no such file");
    assert!(matches!(e, LightmapError::Io(_)), "{e:?}");
}

// ---------------------------------------------------------------------------------------------
// Video planes
// ---------------------------------------------------------------------------------------------

/// Catches a matrix table that maps the BT.601 or BT.2020 code points to BT.709 (wrong colors
/// on most SD and HDR clips), or an unknown code that is not the documented BT.709 fallback.
#[test]
fn h273_matrix_codes() {
    assert_eq!(YuvMatrix::from_h273(1), YuvMatrix::Bt709);
    assert_eq!(YuvMatrix::from_h273(5), YuvMatrix::Bt601);
    assert_eq!(YuvMatrix::from_h273(6), YuvMatrix::Bt601);
    assert_eq!(YuvMatrix::from_h273(9), YuvMatrix::Bt2020Ncl);
    for unknown in [0u8, 2, 3, 4, 7, 8, 10, 14, 255] {
        assert_eq!(YuvMatrix::from_h273(unknown), YuvMatrix::Bt709, "{unknown}");
    }
}

/// Catches conversion rows that do not map limited-range black and white to 0 and 255, or
/// full-range grey off its middle, at every bit depth (and an out-of-range depth clamps).
#[test]
fn convert_rows_map_the_range_ends() {
    let apply =
        |rows: [[f64; 4]; 3], y: f64, u: f64, v: f64| rows.map(|r| r[0] * y + r[1] * u + r[2] * v + r[3]);
    for depth in [8u8, 10, 12, 16] {
        let scale = f64::from(1u32 << (depth - 8));
        for m in [YuvMatrix::Bt601, YuvMatrix::Bt709, YuvMatrix::Bt2020Ncl] {
            let limited = convert_rows(m, false, depth);
            let black = apply(limited, 16.0 * scale, 128.0 * scale, 128.0 * scale);
            let white = apply(limited, 235.0 * scale, 128.0 * scale, 128.0 * scale);
            assert!(black.iter().all(|c| c.abs() < 1e-6), "{m:?} {depth}: {black:?}");
            assert!(
                white.iter().all(|c| (c - 255.0).abs() < 1e-6),
                "{m:?} {depth}: {white:?}"
            );
            let full = convert_rows(m, true, depth);
            let mid = f64::from(1u32 << (depth - 1));
            let max = f64::from((1u32 << depth) - 1);
            let grey = apply(full, max, mid, mid);
            assert!(
                grey.iter().all(|c| (c - 255.0).abs() < 1e-6),
                "{m:?} {depth}: {grey:?}"
            );
        }
    }
    assert_eq!(
        convert_rows(YuvMatrix::Bt709, true, 4),
        convert_rows(YuvMatrix::Bt709, true, 8)
    );
    assert_eq!(
        convert_rows(YuvMatrix::Bt709, true, 40),
        convert_rows(YuvMatrix::Bt709, true, 16)
    );
}

/// Catches a plane check that accepts planes too short for an odd-sized 4:2:0 picture (chroma
/// rounds up), and a converter that reads past them; then a malformed chroma shift of 32 or more
/// from a decoder must be refused, not panic on `1 << 32` (defect fixed in this lane).
#[test]
fn odd_sizes_round_chroma_up_and_bad_shifts_are_refused() {
    fn planes<'a>(y: &'a [u8], uv: &'a [u8], shift: (u32, u32)) -> YuvPlanes<'a> {
        YuvPlanes {
            width: 5,
            height: 3,
            bit_depth: 8,
            chroma_shift: shift,
            matrix: YuvMatrix::Bt709,
            full_range: true,
            planes: [y, uv, uv],
        }
    }
    let y = vec![128u8; 5 * 3];
    let short = vec![128u8; 2];
    let good = vec![128u8; 3 * 2];
    assert_eq!(planes(&y, &good, (1, 1)).chroma_size(), (3, 2));
    assert_eq!(planes(&y, &good, (1, 1)).sample_bytes(), 1);
    assert!(!planes(&y, &short, (1, 1)).is_complete());
    let mut out = vec![1u8; 4];
    assert!(!yuv_to_rgba8(&planes(&y, &short, (1, 1)), &mut out));
    assert!(out.is_empty(), "a refused conversion leaves the output empty");
    assert!(yuv_to_rgba8(&planes(&y, &good, (1, 1)), &mut out));
    assert_eq!(out.len(), 5 * 3 * 4);
    for shift in [(32, 0), (0, 32), (40, 40), (u32::MAX, 1)] {
        let p = planes(&y, &good, shift);
        assert!(!p.is_complete(), "{shift:?}");
        let mut out = Vec::new();
        assert!(!yuv_to_rgba8(&p, &mut out), "{shift:?}");
        let (cw, ch) = p.chroma_size();
        assert!(cw >= 1 && ch >= 1, "{shift:?}");
    }
    let deep = YuvPlanes {
        bit_depth: 10,
        ..planes(&y, &good, (0, 0))
    };
    assert_eq!(deep.sample_bytes(), 2);
}

// ---------------------------------------------------------------------------------------------
// LTC and hair helpers
// ---------------------------------------------------------------------------------------------

/// Catches an edge term that is discontinuous where it switches from its series to `acos`
/// (`a · b = 0.9999`), or that is not finite for equal or opposite vectors.
#[test]
fn ltc_edge_vector_is_continuous_and_finite() {
    let a = Vec3::X;
    let at = |cos: f32| {
        let sin = (1.0 - cos * cos).max(0.0).sqrt();
        edge_vector(a, Vec3::new(cos, sin, 0.0))
    };
    let below = at(0.999_899_9);
    let above = at(0.999_900_1);
    assert!((below - above).length() < 1e-4, "{below:?} vs {above:?}");
    // |θ · (a × b) / sin θ| = θ for unit vectors.
    for cos in [0.9f32, 0.5, 0.0, -0.5, -0.99] {
        let theta = cos.acos();
        assert!((at(cos).length() - theta).abs() < 1e-4, "{cos}");
    }
    assert_eq!(edge_vector(a, a), Vec3::ZERO);
    let opposite = edge_vector(a, -a);
    assert!(opposite.is_finite() && opposite.length() < 1e-3, "{opposite:?}");
}

/// Catches an LTC table upload of the wrong size or with non-finite texels (a black or NaN
/// rect light), and a matrix builder that transposes the table entry.
#[test]
fn ltc_texture_and_matrix_layout() {
    let bytes = ltc_texture_bytes();
    assert_eq!(bytes.len(), (LTC_SIZE * LTC_SIZE * 16) as usize);
    let (floats, rest) = bytes.as_chunks::<4>();
    assert!(rest.is_empty());
    assert!(floats.iter().all(|b| f32::from_le_bytes(*b).is_finite()));
    assert_eq!(minv_from_entry([1.0, 0.0, 0.0, 1.0]), Mat3::IDENTITY);
    // Row major [[a, 0, b], [0, 1, 0], [c, 0, d]]: x' = a x + b z.
    let m = minv_from_entry([2.0, 3.0, 5.0, 7.0]);
    assert_eq!(m * Vec3::X, Vec3::new(2.0, 0.0, 5.0));
    assert_eq!(m * Vec3::Z, Vec3::new(3.0, 0.0, 7.0));
    assert_eq!(m * Vec3::Y, Vec3::Y);
}

/// Catches a hair ramp that divides by a zero span, or breaks when the ramp runs tip to root
/// (end below start); and a strand hash outside 0..=1 for large cell indices.
#[test]
fn hair_ramp_and_hash_limits() {
    let h = |start: f32, end: f32| HairFactors {
        ramp_start: start,
        ramp_end: end,
        ..HairFactors::default()
    };
    assert_eq!(regia_render::hair::ramp_t(&h(0.4, 0.4), 0.9), 0.0);
    assert_eq!(regia_render::hair::ramp_t(&h(0.2, 0.8), 0.5), 0.5);
    assert_eq!(regia_render::hair::ramp_t(&h(0.2, 0.8), -3.0), 0.0);
    assert_eq!(regia_render::hair::ramp_t(&h(0.2, 0.8), 3.0), 1.0);
    let reversed = h(0.8, 0.2);
    assert_eq!(regia_render::hair::ramp_t(&reversed, 0.8), 0.0);
    assert_eq!(regia_render::hair::ramp_t(&reversed, 0.2), 1.0);
    assert!((regia_render::hair::ramp_t(&reversed, 0.5) - 0.5).abs() < 1e-6);
    for cell in [0.0f32, 1.0, -1.0, 17.0, 1.0e3, 1.0e5, -2.5e4, 3.0e6] {
        let v = regia_render::hair::strand_hash(cell);
        assert!((0.0..=1.0).contains(&v), "{cell}: {v}");
        assert_eq!(v, regia_render::hair::strand_hash(cell), "deterministic");
    }
}

/// Catches a shifted tangent that is not unit length, or NaN when the shift cancels the tangent
/// exactly (`T = -shift · N`).
#[test]
fn shifted_tangent_is_unit_or_zero() {
    let t = [1.0, 0.0, 0.0];
    let n = [0.0, 1.0, 0.0];
    let len = |v: [f32; 3]| (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    for shift in [-2.0f32, -0.3, 0.0, 0.3, 5.0] {
        let s = regia_render::hair::shifted_tangent(t, n, shift);
        assert!((len(s) - 1.0).abs() < 1e-5, "{shift}: {s:?}");
    }
    let cancelled = regia_render::hair::shifted_tangent([0.0, -1.0, 0.0], n, 1.0);
    assert!(cancelled.iter().all(|c| c.is_finite()), "{cancelled:?}");
}

// ---------------------------------------------------------------------------------------------
// Thin-film map packer and permutation enforcement
// ---------------------------------------------------------------------------------------------

/// Catches a thin-film map packer that panics on a texture whose pixel buffer is shorter than
/// its size says (a truncated decode), instead of reading the extension's default of 1.
#[test]
fn thin_film_map_reads_255_past_a_short_buffer() {
    let truncated = LabTexture {
        width: 2,
        height: 2,
        rgba: vec![10, 20, 30, 40],
    };
    let packed = pack_thin_film_map(Some(&truncated), None).expect("packed");
    assert_eq!((packed.width, packed.height), (2, 2));
    assert_eq!(packed.rgba.len(), 16);
    assert_eq!(&packed.rgba[..4], &[10, 255, 0, 255]);
    for texel in packed.rgba.as_chunks::<4>().0.iter().skip(1) {
        assert_eq!(texel, &[255, 255, 0, 255]);
    }
    let empty = LabTexture {
        width: 0,
        height: 0,
        rgba: Vec::new(),
    };
    let packed = pack_thin_film_map(None, Some(&empty)).expect("packed");
    assert_eq!(
        (packed.width, packed.height, packed.rgba.as_slice()),
        (1, 1, &[255u8, 255, 0, 255][..])
    );
}

/// Catches enforcement names that do not round-trip through the agent protocol, or a build mode
/// that switches the guard off.
#[test]
fn permutation_enforcement_names_round_trip() {
    for mode in [
        PermutationEnforcement::Off,
        PermutationEnforcement::Warn,
        PermutationEnforcement::Fallback,
    ] {
        assert_eq!(PermutationEnforcement::from_name(mode.name()), Some(mode));
    }
    assert_eq!(PermutationEnforcement::from_name("Warn"), None);
    assert_eq!(PermutationEnforcement::from_name(""), None);
    assert_ne!(PermutationEnforcement::for_build(), PermutationEnforcement::Off);
}
