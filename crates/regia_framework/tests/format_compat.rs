// SPDX-License-Identifier: MPL-2.0
//! Compatibility of the frozen demo, `.frec` and save formats against checked-in fixtures.
//!
//! `tests/fixtures/formats/` holds one file per supported version of each format
//! (`demo/*.regiademo`, `frec/*.frec`, `save/*.save`) and a golden text dump of what the current
//! reader makes of it (`*.golden.txt`). Each test loads a fixture through the public reader,
//! dumps the result and compares it with the golden, so a reader change that alters what an old
//! file means fails here. A file written by a newer engine must be refused with a message that
//! names both versions.
//!
//! **Never regenerate a fixture.** [`FIXTURES`] pins each file's length and FNV-1a hash. A new
//! format version adds new fixtures and keeps the old ones passing. A missing binary fixture or
//! golden is written only when `REGIA_BLESS_FORMAT_FIXTURES=1` is set; an existing file is never
//! overwritten.

use std::fmt::Write as _;
use std::path::PathBuf;

use regia_framework::demo::{DEMO_FORMAT_VERSION, DemoError, DemoRecording};
use regia_framework::input_recording::{FREC_FORMAT_VERSION, decode_frec, encode_frec, load_frec};
use regia_framework::save_game::{
    SAVE_FORMAT_VERSION, SAVE_MAGIC, SaveError, SaveFileRaw, SaveGame, SaveSlot,
};

/// Every checked-in fixture: path under `tests/fixtures/formats/`, byte length, FNV-1a 64.
const FIXTURES: [(&str, usize, u64); 9] = [
    ("demo/v1.regiademo", 733, 0x34b5_30ce_8e47_cfcb),
    ("demo/v2.regiademo", 809, 0xc018_cb4d_52d3_fd16),
    ("demo/v3_future.regiademo", 733, 0xca40_cf8c_0737_d0db),
    ("frec/v1.frec", 217, 0x452a_6d9b_b801_56c6),
    ("frec/v2_future.frec", 217, 0xdf71_69ad_b6e4_9d77),
    ("save/v1_populated.save", 89, 0x8afe_f90a_9d9c_cd85),
    ("save/v2_future.save", 89, 0xc36e_5507_658f_b5e4),
    ("save/v2_refs.save", 116, 0x1e32_1360_85c0_55ea),
    ("save/v3_future.save", 116, 0x316e_1d82_8744_fe05),
];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/formats")
}

fn bless() -> bool {
    std::env::var_os("REGIA_BLESS_FORMAT_FIXTURES").is_some_and(|v| v == "1")
}

fn read(rel: &str) -> Vec<u8> {
    let path = root().join(rel);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn read_text(rel: &str) -> String {
    String::from_utf8(read(rel)).unwrap_or_else(|e| panic!("{rel} is not UTF-8: {e}"))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Writes `bytes` to `rel` only when the file does not exist and blessing is on.
fn write_if_missing(rel: &str, bytes: &[u8]) {
    let path = root().join(rel);
    if bless() && !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("create fixture dir");
        }
        std::fs::write(&path, bytes).expect("write fixture");
    }
}

/// Compares `actual` with the golden `rel`; writes it first when missing and blessing is on.
fn assert_golden(rel: &str, actual: &str) {
    write_if_missing(rel, actual.as_bytes());
    let golden = read_text(rel);
    assert_eq!(
        actual, golden,
        "{rel}: the reader's result differs from the golden dump (a frozen format changed?)"
    );
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// A stable text dump of a demo: every field, floats in bit-exact `{:?}` form.
fn dump_demo(demo: &DemoRecording) -> String {
    let h = &demo.header;
    let mut out = String::new();
    let _ = writeln!(out, "format_version: {}", h.format_version);
    let _ = writeln!(out, "engine_version: {}", h.engine_version);
    let _ = writeln!(out, "build_hash: {:?}", h.build_hash);
    let _ = writeln!(out, "content_hash: {:?}", h.content_hash);
    let _ = writeln!(out, "seed: {}", h.seed);
    let _ = writeln!(out, "level: {}", h.level);
    let _ = writeln!(out, "tick_rate_hz: {}", h.tick_rate_hz);
    let _ = writeln!(out, "step_count: {}", h.step_count);
    let _ = writeln!(out, "keyframe_interval: {}", h.keyframe_interval);
    let _ = writeln!(out, "actions: {:?}", demo.actions);
    for (i, row) in demo.steps.iter().enumerate() {
        let cells: Vec<String> = row
            .iter()
            .map(|v| format!("({:?}, {:?}, {:?})", v.x, v.y, v.z))
            .collect();
        let _ = writeln!(out, "step {i}: {}", cells.join(" "));
    }
    for k in &demo.keyframes {
        let _ = writeln!(
            out,
            "keyframe {}: hash {:016x} snapshot {:?}",
            k.tick, k.state_hash, k.snapshot_hex
        );
    }
    let _ = writeln!(out, "timestamps: {:?}", demo.timestamps);
    out
}

/// A stable text dump of a raw save: the header fields and every payload as hex.
fn dump_save(raw: &SaveFileRaw) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "version: {}", raw.version);
    let _ = writeln!(out, "game_version: {}", raw.game_version);
    let _ = writeln!(out, "engine_version: {}", raw.engine_version);
    let _ = writeln!(out, "timestamp_secs: {:?}", raw.timestamp_secs);
    let _ = writeln!(out, "thumbnail: {:?}", raw.thumbnail.as_deref().map(hex));
    for (i, e) in raw.entities.iter().enumerate() {
        let _ = writeln!(out, "entity {i}:");
        for (name, data) in &e.components {
            let _ = writeln!(out, "  {name} = {}", hex(data));
        }
    }
    for r in &raw.resources {
        let _ = writeln!(out, "resource {} = {}", r.type_path, hex(&r.data));
    }
    // Format 2 (ADR-3239): the save id table, when the file has one.
    if !raw.entity_ids.is_empty() {
        let _ = writeln!(out, "entity_ids: {:?}", raw.entity_ids);
    }
    out
}

/// IEEE CRC-32, the save and `.frec` integrity footer (independent of the engine's copy).
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
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

/// Frames a save payload the way the engine does: magic, version, postcard body, CRC.
fn frame_save<T: serde::Serialize>(version: u32, raw: &T) -> Vec<u8> {
    let payload = postcard::to_allocvec(raw).expect("encode payload");
    let mut out = Vec::new();
    out.extend_from_slice(SAVE_MAGIC);
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&payload);
    out.extend_from_slice(&crc32(&payload).to_le_bytes());
    out
}

/// The postcard layout of a save payload, written out independently of the engine's type
/// (which is `non_exhaustive`): positional, so the field order is the format.
#[derive(serde::Serialize)]
struct MirrorRaw {
    version: u32,
    game_version: String,
    engine_version: String,
    timestamp_secs: f64,
    thumbnail: Option<Vec<u8>>,
    entities: Vec<MirrorEntity>,
    resources: Vec<MirrorResource>,
}

/// Format 2: the format 1 fields, then the save id table (ADR-3239).
#[derive(serde::Serialize)]
struct MirrorRawV2 {
    version: u32,
    game_version: String,
    engine_version: String,
    timestamp_secs: f64,
    thumbnail: Option<Vec<u8>>,
    entities: Vec<MirrorEntity>,
    resources: Vec<MirrorResource>,
    entity_ids: Vec<MirrorEntityId>,
}

/// `SaveEntityId`'s postcard shape: variant index, then the fields.
#[derive(serde::Serialize)]
enum MirrorEntityId {
    Runtime(u32),
    Level { guid: u128, level: u128 },
    Lost,
}

#[derive(serde::Serialize)]
struct MirrorEntity {
    components: std::collections::BTreeMap<String, Vec<u8>>,
}

#[derive(serde::Serialize)]
struct MirrorResource {
    type_path: String,
    data: Vec<u8>,
}

fn populated_save(version: u32) -> MirrorRaw {
    let mut components = std::collections::BTreeMap::new();
    components.insert("Health".to_string(), vec![0x00, 0x00, 0x93, 0x42]);
    components.insert("Transform".to_string(), vec![1, 2, 3, 4, 5, 6]);
    MirrorRaw {
        version,
        game_version: "1.2.3".into(),
        engine_version: "0.2.5".into(),
        timestamp_secs: 1_700_000_000.5,
        thumbnail: Some(vec![0x89, b'P', b'N', b'G']),
        entities: vec![
            MirrorEntity { components },
            MirrorEntity {
                components: Default::default(),
            },
        ],
        resources: vec![MirrorResource {
            type_path: "game::Score".into(),
            data: vec![42, 0, 0, 0],
        }],
    }
}

/// A format 2 save: the populated payload with a table of two saved entities and one external
/// level entity.
fn populated_save_v2(version: u32) -> MirrorRawV2 {
    let v1 = populated_save(version);
    MirrorRawV2 {
        version,
        game_version: v1.game_version,
        engine_version: v1.engine_version,
        timestamp_secs: v1.timestamp_secs,
        thumbnail: v1.thumbnail,
        entities: v1.entities,
        resources: v1.resources,
        entity_ids: vec![
            MirrorEntityId::Level {
                guid: 0x0123_4567_89ab_cdef_0011_2233_4455_6677,
                level: 7,
            },
            MirrorEntityId::Runtime(0),
            MirrorEntityId::Level { guid: 0x77, level: 0 },
            MirrorEntityId::Lost,
        ],
    }
}

fn load_save(rel: &str) -> Result<SaveFileRaw, SaveError> {
    let slot = SaveSlot {
        name: "fixture".into(),
        path: root().join(rel),
        header: None,
    };
    SaveGame::load_raw(&slot, &mut regia_app::prelude::World::new())
}

#[test]
fn bless_binary_fixtures() {
    // No-op unless blessing; writes only files that do not exist yet.
    let v2 = DemoRecording::from_ron(&read_text("demo/v2.regiademo")).expect("v2 demo");
    write_if_missing("frec/v1.frec", &encode_frec(&v2).expect("encode frec"));
    let mut future = encode_frec(&v2).expect("encode frec");
    future[4..8].copy_from_slice(&2_u32.to_le_bytes());
    write_if_missing("frec/v2_future.frec", &future);
    write_if_missing("save/v1_populated.save", &frame_save(1, &populated_save(1)));
    write_if_missing("save/v2_future.save", &frame_save(2, &populated_save(2)));
    write_if_missing("save/v2_refs.save", &frame_save(2, &populated_save_v2(2)));
    write_if_missing("save/v3_future.save", &frame_save(3, &populated_save_v2(3)));
}

#[test]
fn fixtures_are_frozen() {
    for (rel, len, hash) in FIXTURES {
        let bytes = read(rel);
        assert_eq!(
            (bytes.len(), fnv1a(&bytes)),
            (len, hash),
            "{rel} was edited (len, fnv1a)"
        );
    }
}

#[test]
fn every_old_demo_version_migrates_to_the_golden() {
    let v1 = DemoRecording::from_ron(&read_text("demo/v1.regiademo")).expect("v1 loads");
    assert_eq!(
        v1.header.format_version, DEMO_FORMAT_VERSION,
        "migrated in memory"
    );
    assert!(v1.timestamps.is_empty(), "v1 has no timestamps");
    assert_golden("demo/v1.golden.txt", &dump_demo(&v1));

    let v2 = DemoRecording::from_ron(&read_text("demo/v2.regiademo")).expect("v2 loads");
    assert_golden("demo/v2.golden.txt", &dump_demo(&v2));

    // A v1 file rewritten by the current writer reads back to the same recording.
    let rewritten = DemoRecording::from_ron(&v1.to_ron().expect("to_ron")).expect("rewritten loads");
    assert_eq!(dump_demo(&rewritten), dump_demo(&v1));
}

#[test]
fn a_newer_demo_is_refused_with_both_versions_named() {
    let err = DemoRecording::from_ron(&read_text("demo/v3_future.regiademo")).expect_err("v3 refused");
    assert!(matches!(err, DemoError::UnsupportedVersion(3)), "{err:?}");
    assert_eq!(err.code(), "unsupported_version");
    let msg = err.to_string();
    assert!(
        msg.contains('3') && msg.contains("not supported") && msg.contains("1 and 2"),
        "{msg}"
    );
}

#[test]
fn frec_v1_loads_to_the_demo_golden_and_a_newer_one_is_refused() {
    let v1 = decode_frec(&read("frec/v1.frec")).expect("v1 decodes");
    assert_eq!(FREC_FORMAT_VERSION, 1);
    // The record holds the same recording as the v2 RON fixture, so it dumps to its golden.
    assert_golden("frec/v1.golden.txt", &dump_demo(&v1));
    assert_eq!(
        dump_demo(&v1),
        dump_demo(&DemoRecording::from_ron(&read_text("demo/v2.regiademo")).expect("v2"))
    );
    let via_path = load_frec(&root().join("frec/v1.frec")).expect("load_frec");
    assert_eq!(dump_demo(&via_path), dump_demo(&v1));

    let err = decode_frec(&read("frec/v2_future.frec")).expect_err("v2 refused");
    assert!(matches!(err, DemoError::UnsupportedVersion(2)), "{err:?}");
    assert!(err.to_string().contains("not supported"), "{err}");
}

#[test]
fn save_v1_loads_to_the_golden() {
    assert_eq!(
        SAVE_FORMAT_VERSION, 2,
        "a new save version needs a new fixture and migrator input"
    );
    let raw = load_save("save/v1_populated.save").expect("v1 save loads");
    assert_golden("save/v1_populated.golden.txt", &dump_save(&raw));
    // The long-standing minimal fixture still loads through the public path too.
    let empty = load_save("../empty.save").expect("empty save loads");
    assert!(empty.entities.is_empty() && empty.resources.is_empty());
}

#[test]
fn save_v2_loads_to_the_golden() {
    let raw = load_save("save/v2_refs.save").expect("v2 save loads");
    assert_eq!(
        raw.entity_ids.len(),
        4,
        "two saved entities, one external, one lost"
    );
    assert_golden("save/v2_refs.golden.txt", &dump_save(&raw));
}

#[test]
fn a_newer_save_is_refused_with_both_versions_named() {
    let err = load_save("save/v3_future.save").expect_err("v3 refused");
    let msg = err.to_string();
    assert!(
        msg.contains("version 3") && msg.contains("newer") && msg.contains("(2)"),
        "{msg}"
    );
    // The file that stood for "newer" before format 2 existed has the format 1 layout under a
    // version 2 header: it lacks the id table and is refused, never misread.
    let err = load_save("save/v2_future.save").expect_err("a v2 header over a v1 payload");
    assert!(matches!(err, SaveError::Deserialize(_)), "{err:?}");
}

#[test]
fn fixture_dirs_hold_only_listed_files() {
    // Every file under demo/, frec/ and save/ is a fixture or a golden, so none is forgotten.
    for dir in ["demo", "frec", "save"] {
        for entry in std::fs::read_dir(root().join(dir)).expect("read dir").flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = format!("{dir}/{name}");
            let known = FIXTURES.iter().any(|(r, _, _)| *r == rel) || name.ends_with(".golden.txt");
            assert!(known, "{rel} is not in FIXTURES");
        }
    }
}

/// Loads save-file `bytes` through the public file path (from a fresh temporary slot).
fn load_save_bytes(bytes: &[u8]) -> Result<SaveFileRaw, SaveError> {
    let dir = regia_test_utils::unique_temp_dir("format-compat-upgrade");
    let path = dir.join("slot.save");
    std::fs::write(&path, bytes).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    let slot = SaveSlot {
        name: "upgraded".into(),
        path,
        header: None,
    };
    let loaded = SaveGame::load_raw(&slot, &mut regia_app::prelude::World::new());
    let _ = std::fs::remove_dir_all(&dir);
    loaded
}

/// `regia upgrade-project --write` is lossless on every save fixture of every version: a file the
/// loader reads loads to the same payload after the upgrade, and a file it refuses is refused by
/// the upgrade or still refused after it. The upgrade's output is current and idempotent.
#[test]
fn the_save_upgrade_is_lossless_on_every_fixture() {
    use regia_framework::save_entity_refs::upgrade_save_bytes;
    let fixtures = [
        "save/v1_populated.save",
        "save/v2_refs.save",
        "save/v2_future.save",
        "save/v3_future.save",
        "../empty.save",
        "../legacy-brand/v0129.save",
        "../../../../regia_agent/tests/fixtures/format-corpus/save/v1.save",
        "../../../../regia_agent/tests/fixtures/format-corpus/save/v2.save",
    ];
    for rel in fixtures {
        let bytes = read(rel);
        let loaded = load_save_bytes(&bytes);
        match upgrade_save_bytes(&bytes) {
            Ok(upgraded) => {
                assert_eq!(
                    &upgraded[4..8],
                    &SAVE_FORMAT_VERSION.to_le_bytes(),
                    "{rel}: upgraded to the current header"
                );
                assert_eq!(
                    upgrade_save_bytes(&upgraded).ok().as_deref(),
                    Some(upgraded.as_slice()),
                    "{rel}: the upgrade is idempotent"
                );
                match (&loaded, load_save_bytes(&upgraded)) {
                    // The version moves; everything the payload holds stays.
                    (Ok(before), Ok(after)) => assert_eq!(
                        dump_save(&after).lines().skip(1).collect::<Vec<_>>(),
                        dump_save(before).lines().skip(1).collect::<Vec<_>>(),
                        "{rel}: lossless"
                    ),
                    (Err(_), Err(_)) => {}
                    (before, after) => panic!(
                        "{rel}: loaded {:?} before, {:?} after",
                        before.is_ok(),
                        after.is_ok()
                    ),
                }
            }
            Err(_) => assert!(
                loaded.is_err(),
                "{rel}: the loader reads it, the upgrade refused it"
            ),
        }
    }
}

/// The header version word is outside the CRC: a format 2 save whose header was damaged to 0 or
/// 1 still loads (the loader follows the payload's own version), so the upgrade must follow the
/// payload too and keep its id table, never re-read it with the format 1 layout and drop it.
#[test]
fn the_save_upgrade_follows_the_payload_version_over_a_damaged_header() {
    use regia_framework::save_entity_refs::upgrade_save_bytes;
    let bytes = read("save/v2_refs.save");
    let golden = dump_save(&load_save_bytes(&bytes).expect("v2 loads"));
    for header in [0u32, 1] {
        let mut damaged = bytes.clone();
        damaged[4..8].copy_from_slice(&header.to_le_bytes());
        let loaded = load_save_bytes(&damaged).expect("the loader reads a damaged header");
        assert_eq!(
            dump_save(&loaded),
            golden,
            "header {header}: the loader follows the payload"
        );
        let upgraded = upgrade_save_bytes(&damaged).expect("upgrade");
        let after = load_save_bytes(&upgraded).expect("upgraded loads");
        assert_eq!(
            after.entity_ids.len(),
            4,
            "header {header}: the id table survives the upgrade"
        );
        assert_eq!(dump_save(&after), golden, "header {header}: lossless");
    }
}
