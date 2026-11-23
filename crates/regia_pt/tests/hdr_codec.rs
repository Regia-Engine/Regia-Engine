// SPDX-License-Identifier: MPL-2.0
//! Radiance RGBE decoding (`hdr.rs`): the pixel codec against its definition, the scanline forms (flat, adaptive
//! run-length, old-style repeat runs, bottom-up) and the refusals for hostile input. CPU only, no golden.
//!
//! Reference: Greg Ward, "Real Pixels", Graphics Gems II, 1991: a pixel is (r, g, b, e) with value `(c + 0.5)·2^(e − 136)`
//! per component (0 when e is 0); the encoder's mantissas are `c / 2^E · 256` with the largest component's `c / 2^E`
//! in [0.5, 1).

use regia_pt::hdr::{HdrError, decode, encode_flat, read, rgb_to_rgbe, rgbe_to_rgb};

fn header(res: &str) -> Vec<u8> {
    format!("#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n{res}\n").into_bytes()
}

#[test]
fn rgbe_decode_follows_the_definition() {
    assert_eq!(
        rgbe_to_rgb([10, 20, 30, 0]),
        [0.0; 3],
        "exponent 0 is black whatever the mantissas"
    );
    // e = 136: the factor is 1, so each component is mantissa + 0.5.
    assert_eq!(rgbe_to_rgb([0, 1, 255, 136]), [0.5, 1.5, 255.5]);
    // Each exponent step doubles.
    let a = rgbe_to_rgb([100, 100, 100, 130]);
    let b = rgbe_to_rgb([100, 100, 100, 131]);
    assert!((b[0] / a[0] - 2.0).abs() < 1e-6);
}

#[test]
fn rgbe_encode_keeps_the_largest_component_within_a_mantissa_step() {
    for m in [1e-6f32, 0.003, 0.5, 1.0, 3.7, 1000.0, 6.5e4, 1e9] {
        for c in [[m, m, m], [m, m * 0.25, m * 0.01], [m * 0.1, m, 0.0]] {
            let back = rgbe_to_rgb(rgb_to_rgbe(c));
            let tol = m * (1.0 / 128.0); // one mantissa step at the largest component, plus the +0.5 bias
            assert!(
                (back[0].max(back[1]).max(back[2]) - m).abs() <= tol,
                "{c:?} -> {back:?}"
            );
        }
    }
}

#[test]
fn rgbe_encode_maps_degenerate_values_to_black() {
    for c in [
        [0.0; 3],
        [-1.0, -2.0, -3.0],
        [f32::NAN, f32::NAN, f32::NAN],
        [f32::INFINITY, 1.0, 1.0],
        [1e-33, 0.0, 0.0],
    ] {
        assert_eq!(rgb_to_rgbe(c), [0; 4], "{c:?}");
    }
    // A NaN component beside finite ones counts as zero (`f32::max` ignores NaN) and the pixel still encodes.
    let n = rgb_to_rgbe([f32::NAN, 1.0, 1.0]);
    assert_eq!((n[0], n[3] > 0), (0, true));
    // A negative component next to a positive one is clamped to zero, not wrapped.
    let p = rgb_to_rgbe([2.0, -5.0, 1.0]);
    assert_eq!(p[1], 0);
    // The largest finite values saturate the exponent instead of overflowing it.
    assert_eq!(rgb_to_rgbe([f32::MAX, 0.0, 0.0])[3], 255);
}

#[test]
fn flat_files_round_trip_through_the_decoder() {
    let rgb: Vec<[f32; 3]> = (0..12)
        .map(|i| [0.1 + i as f32, 2.0 * i as f32 + 0.5, 100.0 / (1.0 + i as f32)])
        .collect();
    let bytes = encode_flat(4, 3, &rgb);
    let img = decode(&bytes).expect("decodes");
    assert_eq!((img.width, img.height), (4, 3));
    for (got, want) in img.rgb.iter().zip(&rgb) {
        let m = want[0].max(want[1]).max(want[2]);
        for k in 0..3 {
            assert!((got[k] - want[k]).abs() <= m / 128.0, "{got:?} vs {want:?}");
        }
    }
}

#[test]
fn bottom_up_files_flip_and_crlf_blank_lines_end_the_header() {
    // Two rows of one pixel each in a bottom-up file: the first row in the file is the bottom row.
    let mut bytes = b"#?RGBE\r\n\r\n+Y 2 +X 1\n".to_vec();
    bytes.extend_from_slice(&[0, 0, 0, 136]);
    bytes.extend_from_slice(&[10, 10, 10, 136]);
    let img = decode(&bytes).expect("decodes");
    assert_eq!(
        img.rgb[0],
        rgbe_to_rgb([10, 10, 10, 136]),
        "the file's last row is the top row"
    );
    assert_eq!(img.rgb[1], rgbe_to_rgb([0, 0, 0, 136]));
}

#[test]
fn adaptive_run_length_scanlines_decode_runs_and_literals() {
    // A width-8 scanline: header 2, 2, 0, 8, then R, G, B, E each as one run of 8 (count 128 + 8, value).
    let mut bytes = header("-Y 1 +X 8");
    bytes.extend_from_slice(&[2, 2, 0, 8]);
    bytes.extend_from_slice(&[136, 50, 136, 60, 136, 70, 136, 136]);
    let img = decode(&bytes).expect("decodes");
    assert_eq!(img.rgb.len(), 8);
    assert!(img.rgb.iter().all(|p| *p == rgbe_to_rgb([50, 60, 70, 136])));
    // The same width as a literal run for each component: counts below 128 are literals.
    let mut lit = header("-Y 1 +X 8");
    lit.extend_from_slice(&[2, 2, 0, 8]);
    for c in 0..4u8 {
        lit.push(8);
        lit.extend((0..8u8).map(|x| if c == 3 { 136 } else { x * 10 + c }));
    }
    let img = decode(&lit).expect("decodes");
    assert_eq!(img.rgb[3], rgbe_to_rgb([30, 31, 32, 136]));
    // A scanline whose stated width disagrees with the image is refused.
    let mut wrong = header("-Y 1 +X 8");
    wrong.extend_from_slice(&[2, 2, 0, 9]);
    wrong.extend_from_slice(&[0; 40]);
    assert!(matches!(decode(&wrong), Err(HdrError::Format(m)) if m.contains("pixels")));
}

#[test]
fn adaptive_runs_that_overrun_or_stall_are_refused() {
    let base = |body: &[u8]| {
        let mut bytes = header("-Y 1 +X 8");
        bytes.extend_from_slice(&[2, 2, 0, 8]);
        bytes.extend_from_slice(body);
        bytes.extend_from_slice(&[0; 64]);
        decode(&bytes)
    };
    // A repeat run of 9 in a line of 8.
    assert!(matches!(base(&[128 + 9, 1]), Err(HdrError::Format(m)) if m.contains("overruns")));
    // A literal count of zero never advances.
    assert!(matches!(base(&[0, 1]), Err(HdrError::Format(m)) if m.contains("literal")));
    // A literal that would pass the end of the line.
    assert!(matches!(base(&[9, 1, 2, 3]), Err(HdrError::Format(_))));
}

#[test]
fn old_style_repeat_runs_need_a_pixel_to_repeat_and_stack_their_counts() {
    // Width 3: pixel, then `1 1 1 2` repeats it twice.
    let mut ok = header("-Y 1 +X 3");
    ok.extend_from_slice(&[7, 8, 9, 136, 1, 1, 1, 2]);
    let img = decode(&ok).expect("decodes");
    assert!(img.rgb.iter().all(|p| *p == rgbe_to_rgb([7, 8, 9, 136])));
    // A run at the start of a line has nothing to repeat.
    let mut first = header("-Y 1 +X 3");
    first.extend_from_slice(&[1, 1, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert!(matches!(decode(&first), Err(HdrError::Format(m)) if m.contains("old-style")));
    // Consecutive runs shift their counts by 8 bits each: 1 · 256 = 256 pixels of a width-300 line.
    let mut stacked = header("-Y 1 +X 300");
    stacked.extend_from_slice(&[5, 5, 5, 136, 1, 1, 1, 43, 1, 1, 1, 1]);
    // 1 + 43 + 1·256 = 300 pixels.
    let img = decode(&stacked).expect("decodes");
    assert_eq!(img.rgb.len(), 300);
    assert!(img.rgb.iter().all(|p| *p == rgbe_to_rgb([5, 5, 5, 136])));
    // Eight stacked runs would shift the count out of range: refused, not wrapped.
    let mut many = header("-Y 1 +X 4");
    many.extend_from_slice(&[5, 5, 5, 136]);
    for _ in 0..9 {
        many.extend_from_slice(&[1, 1, 1, 0]);
    }
    assert!(decode(&many).is_err());
}

#[test]
fn hostile_headers_and_resolutions_are_refused_with_a_reason() {
    let fmt = |m: &str| matches!(decode_err(m), HdrError::Format(_));
    fn decode_err(bytes: &str) -> HdrError {
        decode(bytes.as_bytes()).expect_err("should be refused")
    }
    assert!(fmt("P6\n1 1\n255\n"), "not a Radiance file");
    assert!(fmt(""), "an empty file");
    assert!(
        fmt("#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n"),
        "no blank line and no resolution"
    );
    assert!(
        fmt("#?RADIANCE\nFORMAT=32-bit_rle_xyze\n\n-Y 1 +X 1\n\0\0\0\0"),
        "xyze is refused"
    );
    for res in [
        "-X 1 +Y 1",
        "-Y 1 -X 1",
        "-Y one +X 1",
        "-Y 1 +X",
        "-Y 0 +X 1",
        "-Y 1 +X 0",
        "-Y 40000 +X 1",
        "-Y 1 +X 40000",
    ] {
        let mut bytes = header(res);
        bytes.extend_from_slice(&[0; 64]);
        assert!(decode(&bytes).is_err(), "resolution {res}");
    }
    // Truncated pixel data: fewer than four bytes per row.
    let mut short = header("-Y 4 +X 1");
    short.extend_from_slice(&[0; 8]);
    assert!(matches!(decode(&short), Err(HdrError::Format(m)) if m.contains("truncated")));
    // A row that starts but ends early.
    let mut cut = header("-Y 1 +X 3");
    cut.extend_from_slice(&[1, 2, 3, 136, 4, 5, 6, 136]);
    assert!(matches!(decode(&cut), Err(HdrError::Format(m)) if m.contains("truncated")));
}

#[test]
fn read_reports_missing_files_as_io_errors_and_reads_written_files() {
    let dir = regia_test_utils::unique_temp_dir("hdr_codec");
    let missing = dir.join("nope.hdr");
    assert!(matches!(read(&missing), Err(HdrError::Io(_))));
    let rgb = vec![[1.0, 2.0, 3.0]; 4];
    let path = dir.join("ok.hdr");
    std::fs::write(&path, encode_flat(2, 2, &rgb)).expect("write");
    let img = read(&path).expect("reads");
    assert_eq!((img.width, img.height), (2, 2));
    assert!((img.rgb[0][2] - 3.0).abs() < 3.0 / 128.0);
    let _ = std::fs::remove_dir_all(&dir);
}
