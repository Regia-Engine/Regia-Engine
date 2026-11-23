// SPDX-License-Identifier: MPL-2.0
//! Test-gap closure for `regia_pt`: EXR decode error paths, `compare::load` / `compare_images`
//! error and fallback paths, `PtHeightfield` sampling, and the `Aov` name table. None of these
//! need a GPU.

use regia_pt::Aov;
use regia_pt::PtError;
use regia_pt::PtHeightfield;
use regia_pt::compare::{self, LoadedImage};
use regia_pt::exr::{self, ExrChannel, ExrError};
use regia_pt::output::{DENOISER_KEY, write_png, write_png_with_text};

fn chan(name: &str, data: Vec<f32>) -> ExrChannel {
    ExrChannel {
        name: name.into(),
        data,
    }
}

fn sample_exr() -> Vec<u8> {
    let plane: Vec<f32> = (0..12).map(|i| i as f32 * 0.5).collect();
    exr::encode(
        4,
        3,
        &[
            chan("beauty.R", plane.clone()),
            chan("beauty.G", plane.clone()),
            chan("beauty.B", plane),
        ],
        &[("regia.seed".into(), "9".into())],
    )
    .expect("encode")
}

fn find(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
        .expect("needle present")
}

// ---------------------------------------------------------------- exr

#[test]
fn exr_every_truncation_of_a_valid_file_is_an_error_not_a_panic() {
    let bytes = sample_exr();
    assert!(exr::decode(&bytes).is_ok());
    for len in 0..bytes.len() {
        assert!(
            exr::decode(&bytes[..len]).is_err(),
            "a {len}-byte prefix of a {}-byte file must not decode",
            bytes.len()
        );
    }
}

#[test]
fn exr_rejects_bad_magic_and_unsupported_versions() {
    let mut bytes = sample_exr();
    let mut bad_magic = bytes.clone();
    bad_magic[0] ^= 0xff;
    assert!(matches!(exr::decode(&bad_magic), Err(ExrError::Format(m)) if m.contains("not an OpenEXR")));

    // Version field bytes 4..8: bit 9 = tiled, bit 12 = multi-part.
    for flag in [0x200u32, 0x1000] {
        let v = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) | flag;
        bytes[4..8].copy_from_slice(&v.to_le_bytes());
        assert!(
            matches!(exr::decode(&bytes), Err(ExrError::Format(m)) if m.contains("not supported")),
            "flag {flag:#x}"
        );
        bytes[4..8].copy_from_slice(&2u32.to_le_bytes());
    }
}

#[test]
fn exr_rejects_compressed_files() {
    let mut bytes = sample_exr();
    // "compression\0compression\0" + u32 size (1) + one value byte.
    let key = b"compression\0compression\0";
    let at = find(&bytes, key) + key.len() + 4;
    bytes[at] = 3; // ZIP
    assert!(matches!(exr::decode(&bytes), Err(ExrError::Format(m)) if m.contains("compressed")));
}

#[test]
fn exr_rejects_unsupported_channel_type() {
    let mut bytes = sample_exr();
    // First channel record: "beauty.B\0" then the pixel type (0 = UINT).
    let key = b"beauty.B\0";
    let at = find(&bytes, key) + key.len();
    bytes[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
    assert!(matches!(exr::decode(&bytes), Err(ExrError::Format(m)) if m.contains("HALF and FLOAT")));
}

#[test]
fn exr_without_a_data_window_is_rejected() {
    let mut bytes = sample_exr();
    // Rename the attribute so the reader never sees a `dataWindow`.
    let at = find(&bytes, b"dataWindow\0");
    bytes[at] = b'X';
    assert!(matches!(exr::decode(&bytes), Err(ExrError::Format(m)) if m.contains("no dataWindow")));
}

#[test]
fn exr_scanline_offset_past_the_file_is_an_error() {
    let mut bytes = sample_exr();
    // The offset table follows the header's terminating null; point entry 0 far past the end.
    let header_end = find(&bytes, b"regia.seed\0string\0") + b"regia.seed\0string\0".len() + 4 + 1 + 1;
    bytes[header_end..header_end + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(exr::decode(&bytes).is_err());
}

#[test]
fn exr_encode_rejects_a_mismatched_channel_and_a_zero_size() {
    let short = chan("a.R", vec![0.0; 5]);
    assert!(matches!(exr::encode(2, 3, &[short], &[]), Err(ExrError::Format(m)) if m.contains("a.R")));
    assert!(exr::encode(3, 0, &[], &[]).is_err());
}

#[test]
fn exr_channels_are_sorted_and_layer_helpers_agree() {
    let bytes = exr::encode(
        2,
        1,
        &[
            chan("z.Y", vec![5.0, 6.0]),
            chan("a.B", vec![3.0, 3.5]),
            chan("a.R", vec![1.0, 1.5]),
            chan("a.G", vec![2.0, 2.5]),
        ],
        &[],
    )
    .expect("encode");
    let img = exr::decode(&bytes).expect("decode");
    let names: Vec<&str> = img.channels.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["a.B", "a.G", "a.R", "z.Y"]);
    assert_eq!(img.layers(), ["a".to_string(), "z".to_string()]);
    assert_eq!(img.rgb("a"), Some(vec![[1.0, 2.0, 3.0], [1.5, 2.5, 3.5]]));
    assert_eq!(img.rgb("z"), None, "a scalar layer has no RGB");
    assert_eq!(img.rgb(""), None, "no plain R/G/B channels");
    assert_eq!(img.channel("missing"), None);
}

#[test]
fn exr_attribute_order_is_preserved() {
    let meta: Vec<(String, String)> = vec![
        ("regia.b".into(), "2".into()),
        ("regia.a".into(), "1".into()),
        ("regia.empty".into(), String::new()),
    ];
    let bytes = exr::encode(1, 1, &[chan("c.Y", vec![1.0])], &meta).expect("encode");
    assert_eq!(exr::decode(&bytes).expect("decode").attributes, meta);
}

#[test]
fn exr_decodes_half_channels() {
    // Hand-built 2x1 file with one HALF channel `Y`: 1.0 and -2.0.
    fn attr(out: &mut Vec<u8>, name: &str, kind: &str, value: &[u8]) {
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        out.extend_from_slice(kind.as_bytes());
        out.push(0);
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(value);
    }
    let mut out = Vec::new();
    out.extend_from_slice(&20_000_630u32.to_le_bytes());
    out.extend_from_slice(&2u32.to_le_bytes());
    let mut chlist = b"Y\0".to_vec();
    chlist.extend_from_slice(&1u32.to_le_bytes()); // HALF
    chlist.extend_from_slice(&[0; 4]);
    chlist.extend_from_slice(&1i32.to_le_bytes());
    chlist.extend_from_slice(&1i32.to_le_bytes());
    chlist.push(0);
    attr(&mut out, "channels", "chlist", &chlist);
    attr(&mut out, "compression", "compression", &[0]);
    let mut window = Vec::new();
    for v in [0i32, 0, 1, 0] {
        window.extend_from_slice(&v.to_le_bytes());
    }
    attr(&mut out, "dataWindow", "box2i", &window);
    out.push(0);
    let chunk = out.len() as u64 + 8;
    out.extend_from_slice(&chunk.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&4u32.to_le_bytes());
    out.extend_from_slice(&0x3c00u16.to_le_bytes());
    out.extend_from_slice(&0xc000u16.to_le_bytes());
    let img = exr::decode(&out).expect("decode");
    assert_eq!((img.width, img.height), (2, 1));
    assert_eq!(img.channel("Y"), Some([1.0f32, -2.0].as_slice()));
}

#[test]
fn exr_read_of_a_missing_file_is_an_io_error_and_write_read_round_trips() {
    let dir = regia_test_utils::unique_temp_dir("regia_pt_exr_io");
    assert!(matches!(exr::read(&dir.join("absent.exr")), Err(ExrError::Io(_))));
    let path = dir.join("x.exr");
    let data = vec![0.25f32, 1e9, -3.0, f32::MAX];
    exr::write(&path, 2, 2, &[chan("v.Y", data.clone())], &[]).expect("write");
    let img = exr::read(&path).expect("read");
    assert_eq!(img.channel("v.Y"), Some(data.as_slice()));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- compare

fn linear(rgb: Vec<[f32; 3]>, w: u32, h: u32) -> LoadedImage {
    LoadedImage {
        width: w,
        height: h,
        rgb,
        linear: true,
        denoiser: None,
    }
}

#[test]
fn compare_load_falls_back_to_plain_rgb_and_scalar_layers() {
    let dir = regia_test_utils::unique_temp_dir("regia_pt_cmp_load");
    let plain = dir.join("plain.exr");
    exr::write(
        &plain,
        2,
        1,
        &[
            chan("R", vec![1.0, 2.0]),
            chan("G", vec![3.0, 4.0]),
            chan("B", vec![5.0, 6.0]),
        ],
        &[],
    )
    .expect("write");
    let img = compare::load(&plain, "beauty").expect("plain R/G/B");
    assert!(img.linear && img.denoiser.is_none());
    assert_eq!(img.rgb, vec![[1.0, 3.0, 5.0], [2.0, 4.0, 6.0]]);

    let scalar = dir.join("depth.exr");
    exr::write(&scalar, 2, 1, &[chan("depth.Y", vec![7.0, 8.0])], &[]).expect("write");
    let img = compare::load(&scalar, "depth").expect("scalar layer");
    assert_eq!(img.rgb, vec![[7.0; 3], [8.0; 3]]);

    let err = compare::load(&scalar, "beauty").expect_err("no such layer");
    assert!(
        matches!(&err, PtError::Io(m) if m.contains("no `beauty` layer")),
        "{err:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn compare_load_reports_a_denoiser_marker_from_exr_and_png() {
    // ADR-8670: the current key, and the legacy key of images written before the rename.
    let legacy = DENOISER_KEY.replacen("regia", regia_core::brand::LEGACY_KEY_PREFIX, 1);
    assert_ne!(legacy, DENOISER_KEY);
    for key in [DENOISER_KEY.to_string(), legacy] {
        denoiser_marker_round_trips(&key);
    }
}

fn denoiser_marker_round_trips(key: &str) {
    let dir = regia_test_utils::unique_temp_dir("regia_pt_cmp_den");
    let exr_path = dir.join("d.exr");
    let marker = vec![(key.to_string(), "oidn test".to_string())];
    exr::write(
        &exr_path,
        1,
        1,
        &[
            chan("beauty.R", vec![1.0]),
            chan("beauty.G", vec![1.0]),
            chan("beauty.B", vec![1.0]),
        ],
        &marker,
    )
    .expect("write");
    assert_eq!(
        compare::load(&exr_path, "beauty")
            .expect("exr")
            .denoiser
            .as_deref(),
        Some("oidn test")
    );
    let png_path = dir.join("d.png");
    write_png_with_text(&png_path, 1, 1, &[10, 20, 30, 255], &marker).expect("png");
    assert_eq!(
        compare::load(&png_path, "beauty")
            .expect("png")
            .denoiser
            .as_deref(),
        Some("oidn test")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn compare_load_png_is_display_encoded_and_errors_are_io() {
    let dir = regia_test_utils::unique_temp_dir("regia_pt_cmp_png");
    let path = dir.join("p.png");
    write_png(&path, 2, 1, &[0, 51, 255, 255, 255, 0, 102, 255]).expect("png");
    let img = compare::load(&path, "beauty").expect("png");
    assert!(!img.linear);
    assert_eq!((img.width, img.height), (2, 1));
    assert_eq!(img.rgb, vec![[0.0, 0.2, 1.0], [1.0, 0.0, 0.4]]);

    assert!(matches!(
        compare::load(&dir.join("absent.png"), "beauty"),
        Err(PtError::Io(_))
    ));
    assert!(matches!(
        compare::load(&dir.join("absent.exr"), "beauty"),
        Err(PtError::Io(_))
    ));
    let junk = dir.join("junk.png");
    std::fs::write(&junk, b"definitely not a png").expect("write");
    assert!(matches!(compare::load(&junk, "beauty"), Err(PtError::Io(_))));
    let junk_exr = dir.join("junk.exr");
    std::fs::write(&junk_exr, b"definitely not an exr").expect("write");
    assert!(matches!(compare::load(&junk_exr, "beauty"), Err(PtError::Io(_))));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn compare_images_rejects_a_size_mismatch() {
    let a = linear(vec![[1.0; 3]; 4], 2, 2);
    let b = linear(vec![[1.0; 3]; 6], 3, 2);
    let err = compare::compare_images(&a, &b, 0.0, 67.0).expect_err("sizes differ");
    assert!(
        matches!(&err, PtError::Scene(m) if m.contains("2x2") && m.contains("3x2")),
        "{err:?}"
    );
}

#[test]
fn compare_images_display_encoded_pair_scores_identical_content_as_equal() {
    // Two display-encoded (PNG-style) images take the non-linear path: identical pixels give
    // zero error and equal means.
    let px = vec![[0.5f32, 0.25, 0.75]; 16];
    let a = LoadedImage {
        linear: false,
        ..linear(px.clone(), 4, 4)
    };
    let b = a.clone();
    let (report, _) = compare::compare_images(&a, &b, 0.0, 67.0).expect("compare");
    assert!(report.flip_mean < 1e-6, "{}", report.flip_mean);
    assert!(report.rmse < 1e-6, "{}", report.rmse);
    assert!((report.mean_reference - report.mean_test).abs() < 1e-6);
    assert!(report.mean_reference > 0.0);
}

#[test]
fn compare_files_mismatched_sizes_fail_and_write_no_map() {
    let dir = regia_test_utils::unique_temp_dir("regia_pt_cmp_files");
    let write = |name: &str, w: u32, h: u32| {
        let n = (w * h) as usize;
        let p = dir.join(name);
        exr::write(
            &p,
            w,
            h,
            &[
                chan("beauty.R", vec![1.0; n]),
                chan("beauty.G", vec![1.0; n]),
                chan("beauty.B", vec![1.0; n]),
            ],
            &[],
        )
        .expect("write");
        p
    };
    let (a, b) = (write("a.exr", 2, 2), write("b.exr", 4, 2));
    let map = dir.join("map.png");
    let err = compare::compare_files(&a, &b, "beauty", 0.0, 67.0, Some(&map)).expect_err("mismatch");
    assert!(matches!(err, PtError::Scene(_)));
    assert!(!map.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- scene / aov

fn grid(width: u32, depth: u32, heights: Vec<f32>) -> PtHeightfield {
    PtHeightfield {
        width,
        depth,
        origin: [10.0, -4.0],
        spacing: [2.0, 4.0],
        heights,
        material: 0,
    }
}

#[test]
fn heightfield_height_clamps_to_the_grid() {
    // 3 x 2 samples: row 0 = 0,1,2; row 1 = 10,11,12.
    let hf = grid(3, 2, vec![0.0, 1.0, 2.0, 10.0, 11.0, 12.0]);
    assert_eq!(hf.height(1, 0), 1.0);
    assert_eq!(hf.height(2, 1), 12.0);
    assert_eq!(hf.height(99, 0), 2.0);
    assert_eq!(hf.height(0, 99), 10.0);
    assert_eq!(hf.height(99, 99), 12.0);
}

#[test]
fn heightfield_height_at_hits_samples_and_rejects_outside() {
    let hf = grid(3, 2, vec![0.0, 1.0, 2.0, 10.0, 11.0, 12.0]);
    // Sample (i, j) sits at origin + (i * 2, j * 4).
    assert_eq!(hf.height_at(10.0, -4.0), Some(0.0));
    assert_eq!(hf.height_at(14.0, 0.0), Some(12.0));
    assert_eq!(hf.height_at(12.0, -4.0), Some(1.0));
    assert_eq!(hf.height_at(9.99, -4.0), None);
    assert_eq!(hf.height_at(14.01, -4.0), None);
    assert_eq!(hf.height_at(10.0, -4.01), None);
    assert_eq!(hf.height_at(10.0, 0.01), None);
    assert_eq!(hf.height_at(f32::NAN, 0.0), None);
}

#[test]
fn heightfield_height_at_follows_the_triangle_diagonal() {
    // One cell, heights (0,0)=0 (1,0)=4 (0,1)=8 (1,1)=12: a plane, so any split agrees.
    let plane = PtHeightfield {
        origin: [0.0, 0.0],
        spacing: [1.0, 1.0],
        ..grid(2, 2, vec![0.0, 4.0, 8.0, 12.0])
    };
    let h = plane.height_at(0.5, 0.5).expect("inside");
    assert!((h - 6.0).abs() < 1e-5, "{h}");
    // A non-planar cell: only (1, 1) is raised. The cell splits along (i, j+1)-(i+1, j), so the
    // lower-left triangle ignores h11 and the upper-right one uses it.
    let bump = PtHeightfield {
        origin: [0.0, 0.0],
        spacing: [1.0, 1.0],
        ..grid(2, 2, vec![0.0, 0.0, 0.0, 6.0])
    };
    assert_eq!(bump.height_at(0.25, 0.25), Some(0.0));
    let h = bump.height_at(0.75, 0.75).expect("inside");
    assert!((h - 3.0).abs() < 1e-5, "{h}");
}

#[test]
fn heightfield_height_at_is_none_for_degenerate_or_short_grids_and_holes() {
    assert_eq!(grid(1, 4, vec![0.0; 4]).height_at(10.0, -4.0), None);
    assert_eq!(grid(4, 1, vec![0.0; 4]).height_at(10.0, -4.0), None);
    assert_eq!(grid(0, 0, Vec::new()).height_at(10.0, -4.0), None);
    // Fewer samples than width * depth.
    assert_eq!(grid(3, 3, vec![0.0; 8]).height_at(10.0, -4.0), None);
    // A cut (NaN) corner drops the whole cell, but a neighbouring clean cell still answers.
    let mut heights = vec![1.0; 9];
    heights[4] = f32::NAN; // centre sample touches all four cells of a 3x3 grid
    let hf = PtHeightfield {
        origin: [0.0, 0.0],
        spacing: [1.0, 1.0],
        ..grid(3, 3, heights)
    };
    assert_eq!(hf.height_at(0.5, 0.5), None);
    assert_eq!(hf.height_at(1.5, 1.5), None);
    let mut heights = vec![1.0; 9];
    heights[8] = f32::NAN; // corner sample touches only the last cell
    let hf = PtHeightfield {
        origin: [0.0, 0.0],
        spacing: [1.0, 1.0],
        ..grid(3, 3, heights)
    };
    assert_eq!(hf.height_at(0.5, 0.5), Some(1.0));
    assert_eq!(hf.height_at(1.5, 1.5), None);
}

#[test]
fn aov_names_round_trip_and_scalar_flags_match_the_exr_layout() {
    let mut seen = std::collections::HashSet::new();
    for aov in Aov::ALL {
        assert!(seen.insert(aov.name()), "duplicate name {}", aov.name());
        assert_eq!(Aov::from_name(aov.name()), Some(aov));
        assert!(
            aov.name().bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
            "{}",
            aov.name()
        );
    }
    assert_eq!(Aov::from_name("Beauty"), None);
    assert_eq!(Aov::from_name(""), None);
    let scalars: Vec<&str> = Aov::ALL
        .into_iter()
        .filter(|a| a.is_scalar())
        .map(Aov::name)
        .collect();
    assert_eq!(scalars, ["depth", "shadow", "spp"]);
}
