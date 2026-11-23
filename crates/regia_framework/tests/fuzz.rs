// SPDX-License-Identifier: MPL-2.0
//! M18 robustness: mutation fuzzing of the save-file loader. See `docs/evidence/m18/robustness.md`.

use regia_framework::save_game::{SaveGame, SaveSpec};
use regia_test_utils::fuzz::fuzz_loader;
use regia_test_utils::regia::ecs::prelude::*;
use regia_test_utils::regia::ecs::reflect::AppTypeRegistry;

const N: usize = 1_000;

#[test]
fn save_file_never_panics() {
    let fixture = include_bytes!("fixtures/empty.save").to_vec();
    fuzz_loader(&fixture, 0x21, N, |bytes| {
        // A fresh world per mutation: the loader mutates the world (spawning saved entities), so
        // reusing one world would let an earlier mutation corrupt the later run's state.
        let mut world = World::new();
        world.insert_resource(AppTypeRegistry::default());
        SaveGame::decode(&mut world, bytes, &SaveSpec::default())
    });
}

/// A cooked table comes from a pak, which a player can edit (ADR-2012): every mutation is a table
/// that passed validation or an error.
#[test]
fn cooked_curve_table_never_panics() {
    use regia_framework::curve_table::CurveTable;
    let table = CurveTable::from_ron_str(
        r#"(format_version: 1, interp: Cubic, curves: {
            "a": (post: Cycle, keys: [(0.0, 1.0), (x: 1.0, y: 2.0, interp: Step, out_tangent: 0.5)]),
            "b": (keys: [(0.0, 0.0)]),
        })"#,
    )
    .expect("fixture");
    let fixture = table.to_cooked().expect("cook");
    fuzz_loader(&fixture, 0x23, N, CurveTable::from_cooked);
}

#[test]
fn cooked_chooser_never_panics() {
    use regia_framework::chooser::ChooserTable;
    let table = ChooserTable::from_ron_str(
        r#"(format_version: 1, mode: Weighted, result: Asset("clip"),
            inputs: { "on": Bool, "n": Int, "t": Tags, "e": Enum(["a", "b"]) },
            outputs: { "rate": Float },
            columns: [
                (id: "on", input: "on", kind: Bool), (id: "n", input: "n", kind: IntRange),
                (id: "t", input: "t", kind: Tags), (id: "e", input: "e", kind: Enum),
                (id: "w", kind: Weight), (id: "r", output: "rate", kind: SetFloat),
            ],
            rows: [
                (id: "a", cells: { "on": True, "n": (0, 9), "e": OneOf(["a", "b"]), "w": 1.5, "r": 2.0,
                    "t": AllOf([Has("A.B"), NoneOf([HasExact("C")])]) }, result: Asset("a/x.clip.ron")),
                (id: "b", cells: {}, result: Inline((result: Asset("clip"),
                    rows: [(id: "c", cells: {}, result: Chooser("n.chooser.ron"))]))),
            ],
            catch_all: Some((result: Asset("a/y.clip.ron"), outputs: { "rate": 1.0 })),
        )"#,
    )
    .expect("fixture");
    let fixture = table.to_cooked().expect("cook");
    fuzz_loader(&fixture, 0x24, N, ChooserTable::from_cooked);
}

#[test]
fn shots_ron_never_panics() {
    use regia_framework::shots::ShotSequence;
    use regia_test_utils::fuzz::fuzz_text_loader;
    let fixture = include_str!("../../../docs/formats/shots.example.ron");
    ShotSequence::from_ron(fixture).expect("the example itself loads");
    fuzz_text_loader(fixture, 0x22, N, ShotSequence::from_ron);
}

/// Table-driven hostile `.shots.ron` documents: each must be a clean error.
#[test]
fn shots_ron_hostile_inputs_are_errors() {
    use regia_framework::shots::ShotSequence;
    let cases = [
        ("empty", ""),
        ("no shots", "(version: 1, shots: [])"),
        ("future version", "(version: 4294967295, shots: [])"),
        (
            "nan duration",
            "(version: 1, shots: [(name: \"a\", duration: NaN)])",
        ),
        (
            "negative duration",
            "(version: 1, shots: [(name: \"a\", duration: -1.0)])",
        ),
        (
            "empty shot name",
            "(version: 1, shots: [(name: \"\", duration: 1e308)])",
        ),
        (
            "nan fps",
            "(version: 1, fps: NaN, shots: [(name: \"a\", duration: 1.0)])",
        ),
        (
            "duplicate names",
            "(version: 1, shots: [(name: \"a\", duration: 1.0), (name: \"a\", duration: 1.0)])",
        ),
        ("unclosed", "(version: 1, shots: [(name: \"a\", duration: 1.0"),
    ];
    for (what, source) in cases {
        let err = ShotSequence::from_ron(source).expect_err(what);
        assert!(!err.to_string().is_empty(), "{what}: empty error message");
    }
}

#[test]
fn seq_ron_never_panics() {
    use regia_framework::sequencer::Sequence;
    use regia_test_utils::fuzz::fuzz_text_loader;
    let fixture = include_str!("../../../docs/formats/seq.example.ron");
    Sequence::from_ron(fixture).expect("the example itself loads");
    fuzz_text_loader(fixture, 0x24, N, Sequence::from_ron);
}

/// Table-driven hostile `.seq.ron` documents: each must be a clean error.
#[test]
fn seq_ron_hostile_inputs_are_errors() {
    use regia_framework::sequencer::Sequence;
    let cases = [
        ("empty", ""),
        // v6 is current since ADR-8105 (event tracks, the track lock); the next version is the
        // future one (also checked against `SEQUENCE_FORMAT_VERSION` below, so the next bump cannot
        // turn this case into a valid file).
        ("future version minimal", "(version: 7)"),
        ("future version", "(version: 4294967295, tracks: [])"),
        (
            "nan key time",
            "(version: 1, tracks: [(name: \"a\", target: \"ui\", property: \"x\", keys: Float([(time: NaN, value: 1.0)])])",
        ),
        (
            "unsorted keys",
            "(version: 1, tracks: [(name: \"a\", target: \"ui\", property: \"x\", keys: Float([(time: 2.0, value: 1.0), (time: 1.0, value: 0.0)])])",
        ),
        (
            "empty key list",
            "(version: 1, tracks: [(name: \"a\", target: \"ui\", property: \"x\", keys: Float([])])",
        ),
        (
            "empty target",
            "(version: 1, tracks: [(name: \"a\", target: \"\", property: \"x\", keys: Bool([(time: 0.0, value: true)])])",
        ),
        (
            "duplicate track names",
            "(version: 1, tracks: [(name: \"a\", target: \"ui\", property: \"x\", keys: Bool([(time: 0.0, value: true)])), (name: \"a\", target: \"ui\", property: \"y\", keys: Bool([(time: 0.0, value: false)]))])",
        ),
        (
            "negative marker time",
            "(version: 1, markers: [(name: \"m\", time: -0.5)])",
        ),
        (
            "nan fps",
            "(version: 1, fps: NaN, tracks: [(name: \"a\", target: \"ui\", property: \"x\", keys: Float([(time: 0.0, value: 1.0)])])",
        ),
        ("unclosed", "(version: 1, tracks: [(name: \"a\""),
        // v2 sections and tangents (ADR-1880).
        (
            "clip section ends before it starts",
            "(version: 2, tracks: [(name: \"c\", target: \"hero\", keys: Clip([(clip: \"walk\", start: 2.0, end: 1.0)]))])",
        ),
        (
            "audio fades longer than the section",
            "(version: 2, tracks: [(name: \"a\", keys: Audio([(sound: \"s\", start: 0.0, end: 1.0, fade_in: 0.8, fade_out: 0.8)]))])",
        ),
        (
            "sub section with zero speed",
            "(version: 2, tracks: [(name: \"s\", keys: Sub([(sequence: \"x\", start: 0.0, end: 1.0, speed: 0.0)]))])",
        ),
        (
            "infinite tangent",
            "(version: 2, tracks: [(name: \"a\", target: \"ui\", property: \"x\", keys: Float([(time: 0.0, value: 1.0, ease: Bezier, tangent: Aligned(inf))]))])",
        ),
        (
            "zero weight",
            "(version: 2, tracks: [(name: \"a\", target: \"ui\", property: \"x\", keys: Float([(time: 0.0, value: 1.0, weights: Some((0.0, 0.5)))]))])",
        ),
        (
            "empty section list",
            "(version: 2, tracks: [(name: \"a\", keys: Audio([]))])",
        ),
    ];
    for (what, source) in cases {
        let err = Sequence::from_ron(source).expect_err(what);
        assert!(!err.to_string().is_empty(), "{what}: empty error message");
    }
    let next = regia_framework::sequencer::SEQUENCE_FORMAT_VERSION + 1;
    let err = Sequence::from_ron(&format!("(version: {next})"))
        .expect_err("the version after the current one is a future version");
    assert!(
        !err.to_string().is_empty(),
        "future version {next}: empty error message"
    );
}

/// A valid v3 replay with a few records, built by the synthetic writer.
fn replay_fixture() -> Vec<u8> {
    use regia_framework::replay_sim::{PilotInput, ReplayHeader, SyntheticReplay};
    let header = ReplayHeader {
        version: 3,
        protocol: "test/1".into(),
        tick_rate: 60,
        lobby_preset: "alpha".into(),
        map: "ironreach".into(),
        ..ReplayHeader::default()
    };
    let input = PilotInput {
        throttle: 10,
        strafe: -3,
        steer: 5,
        aim_pitch: 100,
        action_bits: 0b101,
        stance: 1,
    };
    SyntheticReplay::new(&header)
        .inputs(0, &[(0, input), (1, input)])
        .full(&[1, 0, 5, 2, 1, 7], 0xABCD)
        .inputs(1, &[(0, input)])
        .abort(2, "fuzz")
        .finish()
}

#[test]
fn replay_never_panics() {
    use regia_framework::replay_sim::ReplayFile;
    use regia_test_utils::fuzz::{FuzzRng, assert_no_panic, mutate_bytes};
    let fixture = replay_fixture();
    ReplayFile::parse(&fixture).expect("the fixture itself loads");
    // Every prefix: a cut mid-record is an error, a cut on a record boundary is a shorter replay;
    // neither may panic.
    for keep in 0..fixture.len() {
        let prefix = &fixture[..keep];
        let parsed = assert_no_panic("a truncated replay", || ReplayFile::parse(prefix).is_ok());
        if keep < 8 {
            assert!(!parsed, "a {keep}-byte prefix has no complete header");
        }
    }
    let mut rng = FuzzRng::new(0x23);
    let mut rejected = 0usize;
    for _ in 0..N {
        let mutated = mutate_bytes(&fixture, &mut rng);
        match assert_no_panic("a mutated replay", || ReplayFile::parse(&mutated)) {
            Ok(_) => {}
            Err(e) => {
                assert!(!e.to_string().is_empty());
                rejected += 1;
            }
        }
    }
    assert!(rejected > 0, "the mutations never produced an error");
}

/// Oversized length fields in replay records must be errors, not allocations.
#[test]
fn replay_oversized_lengths_are_errors() {
    use regia_framework::replay_sim::{ReplayFile, ReplayHeader, SyntheticReplay};
    let header = ReplayHeader {
        version: 3,
        protocol: "test/1".into(),
        tick_rate: 60,
        lobby_preset: "alpha".into(),
        ..ReplayHeader::default()
    };
    let head = SyntheticReplay::new(&header);
    let huge = u32::MAX.to_le_bytes();
    // (tag, tick) then a u32 body length of u32::MAX with no body behind it.
    let mut full = vec![0x02u8, 0, 0, 0, 0, 0, 0x01];
    full.extend_from_slice(&huge);
    let mut nav = vec![0x04u8, 0, 0, 0, 0];
    nav.extend_from_slice(&huge);
    let moments = [0x06u8, 0, 0, 0, 0, 0xFF, 0xFF];
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("full record body length u32::MAX", full),
        ("nav record body length u32::MAX", nav),
        ("moment count u16::MAX with no bodies", moments.to_vec()),
        ("unknown tag", vec![0xEE]),
        ("string length past the end", vec![0x07, 0, 0, 0, 0, 0xFF, 0xFF]),
    ];
    for (what, tail) in cases {
        let bytes = head.clone().raw(&tail).finish();
        let err = ReplayFile::parse(&bytes).expect_err(what);
        assert!(!err.to_string().is_empty(), "{what}: empty error message");
    }
}

/// CRC-32 (IEEE) of `data`; the save footer. Copied here because the engine's is private.
fn save_crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Frame `payload` like a save file: the magic and version of `empty.save`, the payload, a CRC.
fn frame_save(payload: &[u8]) -> Vec<u8> {
    let template = include_bytes!("fixtures/empty.save");
    let mut out = template[..8].to_vec();
    out.extend_from_slice(payload);
    out.extend_from_slice(&save_crc32(payload).to_le_bytes());
    out
}

fn decode_fresh(bytes: &[u8]) -> Result<(), String> {
    // A fresh world per call: decode despawns and spawns, so worlds must not be shared.
    let mut world = World::new();
    world.insert_resource(AppTypeRegistry::default());
    SaveGame::decode(&mut world, bytes, &SaveSpec::default())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The CRC footer rejects a plain bit flip, so the save fuzz above rarely reaches the payload
/// decoder. Here the payload is mutated first and the CRC recomputed, so the postcard decoder and
/// the entity/resource application see hostile bytes.
#[test]
fn save_payload_with_valid_crc_never_panics() {
    use regia_test_utils::fuzz::{FuzzRng, assert_no_panic, mutate_bytes};
    use std::collections::BTreeMap;
    // `SaveFileRaw` is `#[non_exhaustive]`, so build its postcard shape (fields in order, structs
    // as tuples): version, game version, engine version, timestamp, thumbnail, entities, resources.
    let raw = (
        1u32,
        "0.0.1",
        "test",
        1.0,
        Some(vec![1, 2, 3]),
        vec![(BTreeMap::from([("Transform".to_string(), vec![0u8; 12])]),)],
        vec![("no::such::Resource", vec![9, 9, 9])],
    );
    let payload = postcard::to_allocvec(&raw).expect("serialize the save payload");
    let valid = frame_save(&payload);
    let _ = assert_no_panic("the unmutated payload", || decode_fresh(&valid));
    let mut rng = FuzzRng::new(0x24);
    let mut rejected = 0usize;
    for _ in 0..N {
        let mutated = frame_save(&mutate_bytes(&payload, &mut rng));
        if assert_no_panic("a mutated save payload", || decode_fresh(&mutated)).is_err() {
            rejected += 1;
        }
    }
    assert!(rejected > 0, "the mutations never produced an error");
}

/// Table-driven hostile save files: each must be a clean error.
#[test]
fn save_hostile_inputs_are_errors() {
    let valid = include_bytes!("fixtures/empty.save").to_vec();
    let mut newer = valid.clone();
    newer[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("eleven bytes", vec![0; 11]),
        ("bad magic", vec![0xFF; 64]),
        ("newer version", newer),
        ("truncated", valid[..valid.len() - 1].to_vec()),
        // Valid framing around payloads that cannot decode:
        (
            "string length u32::MAX",
            frame_save(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]),
        ),
        ("empty payload", frame_save(&[])),
        (
            "huge entity count",
            frame_save(&[1, 0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F]),
        ),
    ];
    for (what, bytes) in cases {
        let err = decode_fresh(&bytes).expect_err(what);
        assert!(!err.is_empty(), "{what}: empty error message");
    }
}
