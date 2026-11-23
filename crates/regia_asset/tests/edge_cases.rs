// SPDX-License-Identifier: MPL-2.0
//! Edge-case tests for the public surface of `regia_asset` (test-quality lane, ADR-1737): empty and
//! hostile inputs, NaN/inf, overflow, round trips and determinism of repeated runs. Everything
//! here is deterministic: no clocks, no sleeps, fixed seeds.

use std::path::Path;

use regia_asset::csv;
use regia_asset::lut_cube;
use regia_asset::registry::{automatic_tags, path_glob_match, tag_key_problem};
use regia_asset::{
    AssetId, AssetManifest, AssetWatchStatus, Assets, COOKED_FORMAT_VERSION, CookedHeader, CookedIoError,
    Handle, LOD_UNSET, LoadKind, LoadSet, LoadSets, LodSwitch, ManifestEntry, SourceKind, WatchEntry,
    WatchOutcome, decode_cooked_file, display_path, encode_cooked_file, lod_at, lod_threshold_scale,
    normalise_asset_path, pack_importer_version, projected_screen_size, screen_size_for_error, step_lod,
    thresholds_from_errors,
};
use regia_core::id::TypePathHash;
use regia_test_utils::unique_temp_dir;

/// Tiny fixed-seed generator (splitmix64) for the property-style tests.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

// ---- csv ----------------------------------------------------------------------------------------

/// A record of one empty field used to vanish on a round trip (the writer emitted a bare line,
/// which the reader treats as no record). Regression test for the CSV writer.
#[test]
fn csv_single_empty_field_record_survives_a_round_trip() {
    let text = csv::to_string(&[vec![""]]);
    assert_eq!(text, "\"\"\r\n");
    let back = csv::parse(&text).expect("parse");
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].fields, vec![String::new()]);

    // Between other records too, and with the one-column form of a header-less table.
    let rows = vec![vec!["a"], vec![""], vec!["c"]];
    let back = csv::parse(&csv::to_string(&rows)).expect("parse");
    let got: Vec<Vec<String>> = back.into_iter().map(|r| r.fields).collect();
    assert_eq!(got, vec![vec!["a"], vec![""], vec!["c"]]);
}

/// Two or more empty fields were never ambiguous (`,` separators keep the record non-empty).
#[test]
fn csv_multiple_empty_fields_round_trip() {
    let rows = vec![vec!["", ""], vec!["", "x", ""]];
    let text = csv::to_string(&rows);
    let back = csv::parse_with(&text, csv::CsvOptions { equal_fields: false }).expect("parse");
    let got: Vec<Vec<String>> = back.into_iter().map(|r| r.fields).collect();
    assert_eq!(got, vec![vec!["", ""], vec!["", "x", ""]]);
}

/// Seeded random fields drawn from a hostile alphabet survive write -> read, and the writer is
/// deterministic.
#[test]
fn csv_random_round_trip_is_exact_and_repeatable() {
    let alphabet: Vec<char> = vec!['a', 'b', ' ', '\t', ',', '"', '\r', '\n', 'é', 'ゲ', '\'', ';'];
    let mut rng = Rng(0xC5F_2026);
    for _case in 0..200 {
        let cols = 1 + rng.below(4) as usize;
        let nrows = 1 + rng.below(5) as usize;
        let rows: Vec<Vec<String>> = (0..nrows)
            .map(|_| {
                (0..cols)
                    .map(|_| {
                        let len = rng.below(6) as usize;
                        (0..len)
                            .map(|_| alphabet[rng.below(alphabet.len() as u64) as usize])
                            .collect()
                    })
                    .collect()
            })
            .collect();
        let text = csv::to_string(&rows);
        assert_eq!(text, csv::to_string(&rows), "writer must be deterministic");
        let back: Vec<Vec<String>> = csv::parse(&text)
            .unwrap_or_else(|e| panic!("{e} for {rows:?} -> {text:?}"))
            .into_iter()
            .map(|r| r.fields)
            .collect();
        assert_eq!(back, rows, "text {text:?}");
    }
}

/// A BOM only, CRs only, a lone quote pair and a field that is only `""""` (one quote).
#[test]
fn csv_degenerate_inputs() {
    assert!(csv::parse("\u{feff}").expect("bom only").is_empty());
    assert!(csv::parse("\r\r\n\n").expect("breaks only").is_empty());
    let one = csv::parse("\"\"").expect("empty quoted");
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].fields, vec![String::new()]);
    let quote = csv::parse("\"\"\"\"").expect("one quote");
    assert_eq!(quote[0].fields, vec!["\"".to_string()]);
    // A trailing comma opens one more empty field, also right before the end of the text.
    assert_eq!(csv::parse("a,").expect("trailing comma")[0].fields, vec!["a", ""]);
    assert_eq!(
        csv::parse("\"a\",").expect("quoted trailing")[0].fields,
        vec!["a", ""]
    );
}

/// Line numbers stay right across quoted CR, CRLF and lone-CR line breaks.
#[test]
fn csv_line_numbers_count_every_break_style() {
    let text = "a,b\r\"x\ry\",\"z\r\nw\"\nc,d";
    let records = csv::parse(text).expect("parse");
    let lines: Vec<usize> = records.iter().map(|r| r.line).collect();
    // Record 1 on line 1; record 2 starts on line 2 and spans two quoted breaks; record 3 follows.
    assert_eq!(lines, vec![1, 2, 5]);
}

/// An error in the middle of a quoted field names the line the quote opened on.
#[test]
fn csv_unterminated_quote_names_the_opening_line() {
    let err = csv::parse("a,b\nc,\"d\ne\nf").expect_err("unterminated");
    assert_eq!(err.kind, csv::CsvErrorKind::UnterminatedQuote);
    assert_eq!(err.line, 2);
}

// ---- cooked wrapper -----------------------------------------------------------------------------

fn header(payload_len: u64) -> CookedHeader {
    CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: TypePathHash::of("regia_asset::CpuMesh"),
        asset_id: AssetId::sub_asset(AssetId::NIL, "cooked"),
        importer_version: pack_importer_version(1, 0, 0),
        dependencies: Vec::new(),
        payload_len,
    }
}

fn raw_file(header: &CookedHeader, payload: &[u8]) -> Vec<u8> {
    let mut out = regia_asset::COOKED_MAGIC.to_vec();
    out.extend_from_slice(&postcard::to_allocvec(header).expect("header"));
    out.extend_from_slice(payload);
    out
}

/// A declared payload length of `u64::MAX` is a clean error, not a wrapped slice or a panic.
#[test]
fn cooked_huge_declared_payload_is_rejected() {
    let file = raw_file(&header(u64::MAX), b"abc");
    match decode_cooked_file(&file) {
        Err(CookedIoError::PayloadLength { expected, actual }) => {
            assert_eq!(expected, u64::MAX);
            assert_eq!(actual, 3);
        }
        other => panic!("expected PayloadLength, got {other:?}"),
    }
}

/// A header cut short anywhere is an error (never a panic), and every prefix of a valid file is
/// either rejected or (only for the whole file) decodes.
#[test]
fn cooked_every_truncation_is_an_error() {
    let h = CookedHeader {
        dependencies: vec![AssetId::sub_asset(AssetId::NIL, "dep")],
        ..header(4)
    };
    let file = encode_cooked_file(&h, b"wxyz").expect("encode");
    for cut in 0..file.len() {
        assert!(
            decode_cooked_file(&file[..cut]).is_err(),
            "prefix of {cut} bytes decoded"
        );
    }
    let (back, payload) = decode_cooked_file(&file).expect("whole file");
    assert_eq!(back, h);
    assert_eq!(payload, b"wxyz");
}

/// Bytes after the declared payload are refused (they were silently ignored before, audit wave
/// 10c stream L1), and an empty payload is fine.
#[test]
fn cooked_trailing_bytes_and_empty_payload() {
    let file = raw_file(&header(0), b"trailing");
    match decode_cooked_file(&file) {
        Err(CookedIoError::PayloadLength { expected, actual }) => {
            assert_eq!((expected, actual), (0, 8));
        }
        other => panic!("expected PayloadLength, got {other:?}"),
    }
    let empty = raw_file(&header(0), b"");
    let (_, payload) = decode_cooked_file(&empty).expect("decode");
    assert!(payload.is_empty());
}

/// `pack_importer_version` saturates instead of overflowing (a panic in debug, a smaller wrapped
/// version in release) and stays monotonic in each component.
#[test]
fn importer_version_saturates() {
    assert_eq!(pack_importer_version(4295, 0, 0), u32::MAX);
    assert_eq!(pack_importer_version(u32::MAX, u32::MAX, u32::MAX), u32::MAX);
    assert_eq!(pack_importer_version(0, u32::MAX, 0), u32::MAX);
    assert_eq!(pack_importer_version(4294, 967, 295), 4_294_967_295);
    assert!(pack_importer_version(2, 0, 0) > pack_importer_version(1, 999, 999));
}

// ---- manifest / paths ---------------------------------------------------------------------------

fn entry(path: &str, tag: &str) -> ManifestEntry {
    ManifestEntry {
        path: path.to_string(),
        id: AssetId::sub_asset(AssetId::NIL, tag),
        type_path: "t".to_string(),
    }
}

/// With a duplicated path the earlier entry resolves, before and after an encode/decode, whatever
/// the other entries are.
#[test]
fn manifest_duplicate_path_keeps_the_first_entry_through_encoding() {
    let m = AssetManifest {
        entries: vec![
            entry("b.png", "b"),
            entry("a.png", "first"),
            entry("a.png", "second"),
        ],
    };
    let first = AssetId::sub_asset(AssetId::NIL, "first");
    assert_eq!(m.id_for_path("a.png"), Some(first));
    let back = AssetManifest::from_postcard(&m.to_postcard().expect("encode")).expect("decode");
    assert_eq!(back.id_for_path("a.png"), Some(first));
    assert_eq!(back.id_for_path("missing.png"), None);
    assert_eq!(m.to_postcard().expect("a"), m.to_postcard().expect("b"));
}

/// A garbage `asset_manifest.bin` is an error from `read_from_cache`; a missing one is `None`; a
/// write leaves no temp file behind.
#[test]
fn manifest_cache_corrupt_missing_and_clean_directory() {
    let dir = unique_temp_dir("tq-manifest");
    assert!(AssetManifest::read_from_cache(&dir).expect("missing").is_none());
    std::fs::write(
        dir.join("asset_manifest.bin"),
        [0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
    )
    .expect("write");
    assert!(AssetManifest::read_from_cache(&dir).is_err());
    let m = AssetManifest {
        entries: vec![entry("x.png", "x")],
    };
    m.write_to_cache(&dir).expect("write");
    let names: Vec<String> = std::fs::read_dir(&dir)
        .expect("read_dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec!["asset_manifest.bin".to_string()],
        "temp file left behind"
    );
    assert_eq!(AssetManifest::read_from_cache(&dir).expect("read"), Some(m));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Writing into a directory that does not exist is an error and leaves nothing behind.
#[test]
fn manifest_write_to_missing_directory_is_an_error() {
    let dir = unique_temp_dir("tq-manifest-missing");
    let gone = dir.join("nope");
    assert!(AssetManifest::default().write_to_cache(&gone).is_err());
    assert!(!gone.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn normalise_asset_path_edges() {
    assert_eq!(normalise_asset_path(Path::new("")), "");
    assert_eq!(normalise_asset_path(Path::new(".")), "");
    assert_eq!(normalise_asset_path(Path::new("./.")), "");
    assert_eq!(normalise_asset_path(Path::new("a//b///c")), "a/b/c");
    assert_eq!(normalise_asset_path(Path::new("a/../b")), "a/../b");
    assert_eq!(normalise_asset_path(Path::new("a/b/")), "a/b");
    assert_eq!(
        normalise_asset_path(Path::new("/a/b")),
        "/a/b",
        "no doubled root separator"
    );
    assert_eq!(normalise_asset_path(Path::new("/")), "/");
}

#[test]
fn display_path_strips_the_project_root_only_when_it_is_a_prefix() {
    let root = Path::new("/proj");
    assert_eq!(
        display_path(root, Path::new("/proj/assets/a.glb")),
        "assets/a.glb"
    );
    assert_eq!(
        display_path(root, Path::new("/other/assets/a.glb")),
        "/other/assets/a.glb"
    );
    // A sibling directory that merely starts with the root's name is not under it.
    assert_eq!(display_path(root, Path::new("/proj2/a.glb")), "/proj2/a.glb");
    assert_eq!(display_path(root, Path::new("/proj")), "");
    assert_eq!(display_path(root, Path::new("rel/a.glb")), "rel/a.glb");
}

// ---- storage ------------------------------------------------------------------------------------

#[test]
fn assets_empty_storage_resolves_nothing() {
    let assets: Assets<u32> = Assets::new();
    assert!(assets.get(&Handle::INVALID).is_none());
    assert!(assets.handle_for_id(AssetId::new_random()).is_none());
    assert!(assets.handle_for_id(AssetId::NIL).is_none());
}

#[test]
fn assets_many_inserts_get_distinct_handles_and_keep_their_values() {
    let mut assets = Assets::new();
    let mut handles = Vec::new();
    for i in 0..500u32 {
        let id = AssetId::sub_asset(AssetId::NIL, &format!("a{i}"));
        handles.push((id, assets.insert(id, i)));
    }
    for (i, (id, handle)) in handles.iter().enumerate() {
        assert_eq!(assets.get(handle), Some(&(i as u32)));
        assert_eq!(assets.handle_for_id(*id), Some(*handle));
    }
    let mut indexes: Vec<u32> = handles.iter().map(|(_, h)| h.index).collect();
    indexes.sort_unstable();
    indexes.dedup();
    assert_eq!(indexes.len(), 500);
}

// ---- LUT ----------------------------------------------------------------------------------------

fn identity_cube() -> lut_cube::CubeLut {
    let mut text = String::from("LUT_3D_SIZE 2\n");
    for b in 0..2 {
        for g in 0..2 {
            for r in 0..2 {
                text.push_str(&format!("{r} {g} {b}\n"));
            }
        }
    }
    lut_cube::parse(&text).expect("identity cube")
}

fn identity_1d() -> lut_cube::CubeLut {
    lut_cube::parse("LUT_1D_SIZE 2\n0 0 0\n1 1 1\n").expect("1d")
}

/// NaN in one channel reads that channel's first table entry; the output is finite and the other
/// channels are unaffected. Infinities clamp to the table edges. (Regression: NaN used to turn
/// the whole sample into NaN in both interpolation methods and for 1D tables.)
#[test]
fn lut_non_finite_inputs_give_finite_outputs() {
    use regia_asset::lut_cube::CubeLutInterpolation::{Tetrahedral, Trilinear};
    use regia_core::glam::Vec3;
    for lut in [identity_cube(), identity_1d()] {
        for method in [Trilinear, Tetrahedral] {
            let out = lut.sample_with(Vec3::new(f32::NAN, 0.5, 0.25), method);
            assert_eq!(out.x, 0.0, "NaN channel reads entry 0");
            assert!(
                (out.y - 0.5).abs() < 1e-6 && (out.z - 0.25).abs() < 1e-6,
                "{out:?}"
            );

            let all_nan = lut.sample_with(Vec3::NAN, method);
            assert!(all_nan.is_finite(), "{all_nan:?}");

            let inf = lut.sample_with(Vec3::new(f32::INFINITY, f32::NEG_INFINITY, 2.0), method);
            assert!(
                (inf - Vec3::new(1.0, 0.0, 1.0)).abs().max_element() < 1e-6,
                "{inf:?}"
            );
        }
    }
}

/// Sampling is a pure function: the same input twice gives bit-identical output.
#[test]
fn lut_sampling_is_deterministic() {
    use regia_core::glam::Vec3;
    let lut = identity_cube();
    let mut rng = Rng(7);
    for _ in 0..100 {
        let v = Vec3::new(
            (rng.below(2000) as f32 - 500.0) / 1000.0,
            (rng.below(2000) as f32 - 500.0) / 1000.0,
            (rng.below(2000) as f32 - 500.0) / 1000.0,
        );
        assert_eq!(
            lut.sample(v).to_array().map(f32::to_bits),
            lut.sample(v).to_array().map(f32::to_bits)
        );
    }
}

// ---- LOD selection ------------------------------------------------------------------------------

/// Seeded random error chains, including zeros, NaN and infinities: the thresholds always end at 0,
/// are strictly decreasing, finite and non-negative.
#[test]
fn lod_thresholds_are_strictly_decreasing_for_hostile_errors() {
    let specials = [
        0.0,
        -1.0,
        f32::NAN,
        f32::INFINITY,
        1e-9,
        1e-6,
        1e30,
        0.001,
        0.01,
        0.5,
        100.0,
    ];
    let mut rng = Rng(0x10D5);
    for _ in 0..500 {
        let n = rng.below(8) as usize;
        let errors: Vec<f32> = (0..n)
            .map(|_| {
                if rng.below(2) == 0 {
                    specials[rng.below(specials.len() as u64) as usize]
                } else {
                    rng.below(100_000) as f32 / 1000.0
                }
            })
            .collect();
        let diameter = [0.0, 1.0, 4.0, f32::NAN, 1e20][rng.below(5) as usize];
        let sizes = thresholds_from_errors(&errors, diameter);
        assert_eq!(sizes.len(), n + 1);
        assert_eq!(*sizes.last().expect("last"), 0.0);
        for s in &sizes {
            assert!(
                s.is_finite() && *s >= 0.0,
                "{sizes:?} from {errors:?}, d={diameter}"
            );
        }
        for pair in sizes.windows(2) {
            assert!(pair[0] > pair[1], "{sizes:?} from {errors:?}, d={diameter}");
        }
    }
}

#[test]
fn lod_helpers_edge_values() {
    assert_eq!(thresholds_from_errors(&[], 1.0), vec![0.0]);
    assert_eq!(lod_at(&[], 5.0), 0);
    assert_eq!(lod_at(&[0.5, 0.0], f32::NAN), 1, "NaN size is the coarsest level");
    assert_eq!(lod_at(&[0.5, 0.0], f32::INFINITY), 0);
    assert_eq!(
        lod_at(&[0.5, 0.0], 0.5),
        0,
        "a size equal to a threshold meets it"
    );
    assert_eq!(projected_screen_size(f32::NAN, 1.0, 1.0), 0.0);
    assert_eq!(projected_screen_size(-1.0, 1.0, 1.0), 0.0);
    assert_eq!(
        projected_screen_size(1.0, f32::NAN, 1.0),
        projected_screen_size(1.0, 0.0, 1.0)
    );
    assert!(projected_screen_size(1.0, 0.0, 0.0).is_finite());
    assert!(projected_screen_size(1.0, 1.0, f32::INFINITY).is_finite());
    assert!(projected_screen_size(f32::MAX, 1e-3, 1e-9).is_finite());
    assert_eq!(lod_threshold_scale(0), 1080.0, "an unknown height scales by 1080");
    assert_eq!(lod_threshold_scale(1080), 1.0);
    assert_eq!(screen_size_for_error(1.0, f32::INFINITY), 1.0e6);
}

/// The same size sequence stepped twice from a fresh state gives the same picks, every pick is in
/// range and every cross-fade coverage lies strictly between 0 and 1.
#[test]
fn step_lod_is_deterministic_and_in_range() {
    let sizes = [0.5, 0.1, 0.02, 0.0];
    let run = |seed: u64| {
        let mut rng = Rng(seed);
        let mut state = LodSwitch::default();
        let mut picks = Vec::new();
        for frame in 1..400u64 {
            let size = rng.below(1000) as f32 / 1000.0;
            let on_screen = rng.below(8) != 0;
            let crossfade = rng.below(4) != 0;
            // An occasional skipped frame exercises the "returning" path.
            let frame = if rng.below(50) == 0 { frame + 5 } else { frame };
            let pick = step_lod(&sizes, size, on_screen, crossfade, 0.15, 6, frame, &mut state);
            assert!(pick.lod < sizes.len());
            if let Some((prev, coverage)) = pick.fade {
                assert!(prev < sizes.len());
                assert!(coverage > 0.0 && coverage < 1.0, "coverage {coverage}");
            }
            picks.push((pick.lod, pick.fade.map(|(p, c)| (p, c.to_bits()))));
        }
        picks
    };
    assert_eq!(run(99), run(99));
    assert_ne!(run(99), run(100));
}

/// A state left over from a longer chain (its LOD past the new chain's end) is repicked, not
/// indexed; an empty chain yields LOD 0 without touching the state.
#[test]
fn step_lod_survives_a_shorter_chain_and_an_empty_one() {
    let mut state = LodSwitch {
        lod: 7,
        prev: 7,
        start: 1,
        seen: 1,
    };
    let pick = step_lod(&[0.5, 0.0], 0.1, true, true, 0.15, 8, 2, &mut state);
    assert_eq!(pick.lod, 1);
    assert!(pick.fade.is_none());

    let mut fresh = LodSwitch::default();
    assert_eq!(fresh.lod, LOD_UNSET);
    let pick = step_lod(&[], 0.1, true, true, 0.15, 8, 1, &mut fresh);
    assert_eq!(pick.lod, 0);
    assert_eq!(fresh, LodSwitch::default());
}

// ---- load sets ----------------------------------------------------------------------------------

/// Huge byte estimates saturate instead of overflowing (a panic in debug builds, a wrapped,
/// tiny total in release builds).
#[test]
fn load_set_byte_totals_saturate() {
    let set = LoadSet::new("big")
        .with(LoadKind::Texture, "a", u64::MAX, 1)
        .with(LoadKind::Texture, "b", u64::MAX, u64::MAX)
        .with(LoadKind::Mesh, "c", 5, 0);
    assert_eq!(set.bytes().0, u64::MAX);
    assert_eq!(set.bytes().1, u64::MAX);

    let sets = LoadSets::default();
    sets.define(set);
    sets.mark_resident(LoadKind::Texture, "a");
    let p = sets.load_progress("big").expect("progress");
    assert_eq!(p.total, 3);
    assert_eq!(p.resident, 1);
    assert!(p.fraction >= 0.0 && p.fraction < 1.0, "fraction {}", p.fraction);
    sets.mark_resident(LoadKind::Texture, "b");
    sets.mark_resident(LoadKind::Mesh, "c");
    assert!(sets.is_resident("big"));
    assert_eq!(sets.load_progress("big").expect("progress").fraction, 1.0);
}

#[test]
fn load_set_empty_unknown_and_duplicates() {
    let sets = LoadSets::default();
    assert!(sets.load_progress("nope").is_none());
    assert!(!sets.is_resident("nope"));
    assert!(sets.get("nope").is_none());
    sets.remove("nope");

    // An empty set is complete (fraction 1, nothing pending).
    sets.define(LoadSet::new("empty"));
    let p = sets.load_progress("empty").expect("progress");
    assert_eq!((p.resident, p.total, p.fraction), (0, 0, 1.0));
    assert!(sets.is_resident("empty"));

    // Duplicate (kind, key) items are ignored; the same key under another kind is distinct.
    let set = LoadSet::new("dup")
        .with(LoadKind::Texture, "k", 1, 1)
        .with(LoadKind::Texture, "k", 100, 100)
        .with(LoadKind::Mesh, "k", 1, 1);
    assert_eq!(set.items.len(), 2);
    assert_eq!(set.bytes(), (2, 2));

    // Redefining a set replaces it; names stay sorted.
    sets.define(set);
    sets.define(LoadSet::new("dup"));
    assert_eq!(sets.get("dup").expect("dup").items.len(), 0);
    assert_eq!(sets.names(), vec!["dup".to_string(), "empty".to_string()]);
}

#[test]
fn load_kind_names_are_unique_and_unknown_names_are_none() {
    let mut names: Vec<&str> = LoadKind::ALL.iter().map(|k| k.name()).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), LoadKind::ALL.len());
    assert_eq!(LoadKind::from_name(""), None);
    assert_eq!(LoadKind::from_name("Texture"), None, "names are case sensitive");
}

// ---- live reload bookkeeping --------------------------------------------------------------------

#[test]
fn source_kind_edge_names() {
    assert_eq!(SourceKind::of(Path::new("")), SourceKind::Other);
    assert_eq!(SourceKind::of(Path::new("noextension")), SourceKind::Other);
    assert_eq!(SourceKind::of(Path::new("a.ron")), SourceKind::Other);
    assert_eq!(SourceKind::of(Path::new("a.material.ron.bak")), SourceKind::Other);
    assert_eq!(SourceKind::of(Path::new("a.glb.meta")), SourceKind::Other);
    assert_eq!(SourceKind::of(Path::new("DIR.glb/file.txt")), SourceKind::Other);
    assert_eq!(
        SourceKind::of(Path::new("x/terrain.layers.ron")),
        SourceKind::TerrainLayers
    );
    assert_eq!(SourceKind::of(Path::new("A.WGSL")), SourceKind::Shader);
}

/// `as_str` is distinct per kind, and the cooked/runtime-read split covers every kind: a kind
/// the cooker produces blobs for is never also silently ignored.
#[test]
fn source_kind_names_are_unique_and_every_kind_has_a_consumer() {
    let samples = [
        "a.glb",
        "a.png",
        "a.material.ron",
        "a.matgraph.ron",
        "a.matinst.ron",
        "a.matfunc.ron",
        "a.level.ron",
        "a.prefab.ron",
        "terrain.layers.ron",
        "a.scatter.ron",
        "a.decal.ron",
        "a.vfx.ron",
        "a.wav",
        "a.ui.ron",
        "a.wgsl",
        "a.bin",
    ];
    let mut seen = std::collections::BTreeSet::new();
    for sample in samples {
        let kind = SourceKind::of(Path::new(sample));
        assert!(
            seen.insert(kind.as_str()),
            "{sample}: duplicate name {}",
            kind.as_str()
        );
        // Material functions are neither cooked nor read on their own: the material-graph cook
        // inlines them, and the live cooker recooks the graphs that use them.
        if kind != SourceKind::Other && kind != SourceKind::MaterialFunction {
            assert!(
                kind.is_cooked() || kind.is_read_at_runtime(),
                "{sample} ({}) has no consumer",
                kind.as_str()
            );
        }
    }
    assert_eq!(seen.len(), samples.len());
}

#[test]
fn watch_status_ring_drops_the_oldest_and_counts_everything() {
    let mut status = AssetWatchStatus::default();
    assert!(status.last_line().is_none());
    let total = AssetWatchStatus::RECENT + 5;
    for i in 0..total {
        let outcome = if i % 3 == 0 {
            WatchOutcome::Error("boom".into())
        } else {
            WatchOutcome::Reloaded
        };
        let line = status.record(WatchEntry {
            path: format!("assets/{i}.glb"),
            kind: SourceKind::Mesh,
            millis: i as u64,
            outcome,
        });
        assert!(line.contains(&format!("assets/{i}.glb")));
    }
    assert_eq!(status.recent.len(), AssetWatchStatus::RECENT);
    assert_eq!(status.reloads + status.errors, total as u64);
    assert_eq!(status.errors, (0..total).filter(|i| i % 3 == 0).count() as u64);
    assert_eq!(status.recent.front().expect("front").path, "assets/5.glb");
    assert_eq!(
        status.recent.back().expect("back").path,
        format!("assets/{}.glb", total - 1)
    );
    assert_eq!(status.last_line(), status.recent.back().map(WatchEntry::toast));
}

#[test]
fn watch_entry_toast_formats() {
    let ok = WatchEntry {
        path: "assets/a.glb".into(),
        kind: SourceKind::Mesh,
        millis: 120,
        outcome: WatchOutcome::Reloaded,
    };
    assert_eq!(ok.toast(), "reloaded assets/a.glb in 120 ms");
    let bad = WatchEntry {
        outcome: WatchOutcome::Error("bad uv".into()),
        ..ok
    };
    assert_eq!(bad.toast(), "cook error in assets/a.glb: bad uv");
}

// ---- registry tags and globs --------------------------------------------------------------------

#[test]
fn glob_edge_cases() {
    assert!(path_glob_match("", ""));
    assert!(!path_glob_match("", "a"));
    assert!(path_glob_match("**", ""));
    assert!(path_glob_match("**", "a/b/c"));
    assert!(!path_glob_match("*", "a/b"));
    assert!(path_glob_match("*", ""));
    assert!(!path_glob_match("?", ""));
    assert!(!path_glob_match("?", "/"));
    assert!(path_glob_match("assets/**/a.glb", "assets/a.glb"));
    assert!(path_glob_match("assets/**/a.glb", "assets/x/y/a.glb"));
    assert!(path_glob_match("assets/**", "assets"));
    assert!(path_glob_match("é?*", "éxyz"));
    // Pathologically repetitive globs stay linear (no backtracking explosion) and give the answer.
    let glob = "*a".repeat(200) + "b";
    let path = "a".repeat(400);
    assert!(!path_glob_match(&glob, &path));
    // Over the length cap nothing matches, not even the identical text.
    let long = "a".repeat(2000);
    assert!(!path_glob_match(&long, &long));
}

#[test]
fn tag_key_and_automatic_tag_edges() {
    assert_eq!(tag_key_problem(""), Some("is empty"));
    assert_eq!(tag_key_problem("a=b"), Some("contains `=`"));
    assert!(tag_key_problem("a b").is_some());
    assert!(tag_key_problem("a\u{0}").is_some());
    assert!(
        tag_key_problem("a\u{a0}").is_some(),
        "no-break space is whitespace"
    );
    assert!(tag_key_problem("ok_key-1").is_none());
    assert!(tag_key_problem(&"k".repeat(10_000)).is_some());

    let tags = automatic_tags(".hidden");
    assert!(tags.is_empty(), "a dotfile has no extension: {tags:?}");
    assert!(automatic_tags("noext").is_empty());
    assert!(!automatic_tags("trailing.").contains_key("ext"));
    assert_eq!(automatic_tags("assets/a/B.PNG")["ext"], "png");
    assert_eq!(automatic_tags("assets/a/B.PNG")["folder"], "a");
    assert!(
        !automatic_tags("assets/b.glb").contains_key("folder"),
        "root folder is omitted"
    );
    assert_eq!(
        automatic_tags("assets/a.glb#mesh/x.y")["ext"],
        "glb",
        "sub-asset uses its source"
    );
}
