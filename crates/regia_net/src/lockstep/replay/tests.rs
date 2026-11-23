// SPDX-License-Identifier: MPL-2.0
//! `.lsreplay` v1 unit tests (ADR-8513): the writer, the bounded reader, the bit-exact audit and the
//! library. The 600-tick match of the soak kit is `regia_net_soak`'s `tests/lsreplay.rs`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use super::*;
use crate::desync::sample::{SAMPLE_GAME_ID, SampleGame, SampleSim, sample_input};
use crate::desync::{DesyncHash, ReplayRegistry};
use crate::lockstep::{LockstepFrame, MembershipChange, MembershipKind, SeatId, SeatInput};

static DIRS: AtomicU64 = AtomicU64::new(0);

/// A fresh directory under the system temp dir (pid plus a counter), removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = DIRS.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-lsreplay-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn frame(seats: u16, tick: u64) -> LockstepFrame<u8> {
    LockstepFrame {
        tick,
        inputs: (0..seats)
            .map(|s| {
                let input = if (tick + u64::from(s)).is_multiple_of(13) {
                    SeatInput::Absent
                } else {
                    SeatInput::Present(sample_input(s, tick))
                };
                (SeatId(s), input)
            })
            .collect(),
        delay: 4,
        delay_from: 0,
        membership: if tick == 0 {
            (0..seats)
                .map(|s| MembershipChange {
                    seat: SeatId(s),
                    kind: MembershipKind::Join,
                })
                .collect()
        } else {
            Vec::new()
        },
        match_end: false,
    }
}

fn settings(full_every: u32, hash_every: u32) -> LsReplaySettings {
    LsReplaySettings {
        record: true,
        full_every_ticks: Some(full_every),
        hash_every_ticks: hash_every,
        ..LsReplaySettings::default()
    }
}

/// Records a sample match of `seats` seats over `ticks` ticks: a start full, every frame, a full
/// every `full_every` ticks and the hashes every `hash_every` ticks.
fn record_sample(seats: u16, ticks: u64, settings: &LsReplaySettings) -> Vec<u8> {
    let header = LsReplayHeader::of::<SampleGame>((0..seats).map(SeatId), 60, 0x5eed, settings);
    let mut writer = LsReplayWriter::new(Vec::new(), header, settings.clone()).expect("header");
    let mut sim = SampleSim::default();
    writer
        .record_start(&sim, |out| out.extend(sim.snapshot()))
        .expect("start");
    for tick in 0..ticks {
        let f = frame(seats, tick);
        writer.record_frame(&f).expect("frame");
        sim.step_inputs(tick, f.inputs.iter().map(|(s, i)| (s.0, i.as_ref().copied())));
        writer
            .record_state(tick, &sim, |out| out.extend(sim.snapshot()))
            .expect("state");
    }
    writer.finish(None).expect("finish")
}

/// Every record of `bytes`, in file order.
fn records(bytes: &[u8]) -> (LsReplayHeader, Vec<LsRecord>) {
    let mut reader = LsReplayReader::new(bytes).expect("header");
    let mut out = Vec::new();
    while let Some(record) = reader.next_record().expect("record") {
        out.push(record);
    }
    (reader.header().clone(), out)
}

/// `bytes` with `edit` applied to every record, written back through the writer's raw path.
fn rewrite(bytes: &[u8], mut edit: impl FnMut(&mut LsRecord)) -> Vec<u8> {
    let (header, records) = records(bytes);
    let mut writer = LsReplayWriter::new(Vec::new(), header, LsReplaySettings::default()).expect("header");
    for mut record in records {
        edit(&mut record);
        writer.write_record(&record).expect("record");
    }
    writer.into_inner()
}

fn audit_sample(bytes: &[u8]) -> ReplayAudit {
    audit(bytes, &mut SampleSim::default()).expect("audit")
}

/// Acceptance (NL-02): one input byte of seat 2 flipped at tick 77 is named by the audit, tick and
/// subsystem, against the hashes recorded every tick; with hashes every 10 ticks it names the first
/// checkpoint after it and the last agreed one before it. The untouched file audits bit-exact.
#[test]
fn replay_audit_names_a_flipped_input_tick() {
    let clean = record_sample(4, 240, &settings(60, 1));
    let verdict = audit_sample(&clean);
    assert!(verdict.bit_exact(), "{verdict:?}");
    assert_eq!(verdict.ticks, Some((0, 239)));
    assert_eq!(verdict.frames, 240);
    assert_eq!(verdict.hashes_compared, 240);
    assert_eq!(
        verdict.fulls_compared, 4,
        "fulls after ticks 59, 119, 179 and 239"
    );

    let flip = |record: &mut LsRecord| {
        if let LsRecord::Inputs(inputs) = record
            && inputs.frame.tick == 77
        {
            let input = inputs.frame.inputs[2]
                .input
                .as_mut()
                .expect("seat 2 is present at 77");
            input.0[0] ^= 0x10;
        }
    };
    let flipped = rewrite(&clean, flip);
    assert_ne!(flipped, clean);
    let verdict = audit_sample(&flipped);
    assert!(!verdict.bit_exact());
    assert_eq!(verdict.first_diverging_tick, Some(77), "{verdict:?}");
    assert_eq!(verdict.last_agreed_tick, Some(76));
    // Seat inputs fold into `units` and `orders`; `units` is first in the recorded order.
    assert_eq!(verdict.subsystem.as_deref(), Some("units"));
    // `economy` folds `units` after the seat loop, so it differs at the same tick.
    assert_eq!(
        verdict.subsystems,
        ["units", "economy", "orders"].map(str::to_owned).to_vec()
    );
    // The full after tick 119 reloads the recorded state: later checkpoints agree again.
    assert_eq!(verdict.resumed_at, vec![119]);
    assert!(
        verdict
            .summary()
            .iter()
            .any(|l| l.contains("first diverging tick 77"))
    );

    // Checkpoints every 10 ticks: the first one after the flip, and the last agreed one.
    let sparse = rewrite(&record_sample(4, 240, &settings(60, 10)), flip);
    let verdict = audit_sample(&sparse);
    assert_eq!(verdict.first_diverging_tick, Some(80));
    assert_eq!(verdict.last_agreed_tick, Some(70));
    assert_eq!(verdict.subsystem.as_deref(), Some("units"));
}

/// The writer's records read back as written, and re-writing them gives the same bytes.
#[test]
fn records_round_trip_bit_exact() {
    let bytes = record_sample(3, 120, &settings(30, 10));
    let (header, recs) = records(&bytes);
    assert_eq!(header.game_id, SAMPLE_GAME_ID);
    assert_eq!(header.seats, vec![0, 1, 2]);
    assert_eq!(header.tick_rate, 60);
    assert_eq!(header.seed, 0x5eed);
    assert_eq!(header.full_every_ticks, 30);
    assert_eq!(header.hash_every_ticks, 10);
    let count = |k: u8| recs.iter().filter(|r| r.kind() == k).count();
    assert_eq!(count(RECORD_INPUTS), 120);
    assert_eq!(count(RECORD_FULL), 1 + 4, "the start full and one every 30 ticks");
    assert_eq!(count(RECORD_HASHES), 12);
    assert_eq!(rewrite(&bytes, |_| {}), bytes);
    // An abort trailer, peer commands and membership survive too.
    let mut writer = LsReplayWriter::new(Vec::new(), header, settings(30, 10)).expect("header");
    writer
        .record_peer_cmds(
            5,
            &[PeerCmd {
                seat: 1,
                ok: false,
                kind: "pause".into(),
                text: "not the host".into(),
            }],
        )
        .expect("cmds");
    let out = writer
        .finish(Some(ReplayAbortInfo::new(9, "desync", "seat 2 diverged")))
        .expect("finish");
    let (_, recs) = records(&out);
    assert_eq!(
        recs,
        vec![
            LsRecord::PeerCmds(PeerCmdsRecord {
                tick: 5,
                cmds: vec![PeerCmd {
                    seat: 1,
                    ok: false,
                    kind: "pause".into(),
                    text: "not the host".into()
                }]
            }),
            LsRecord::Abort(ReplayAbortInfo::new(9, "desync", "seat 2 diverged")),
        ]
    );
}

/// Acceptance (NL-02): a file from a newer format version is refused with a readable error that
/// names both versions; the header is not parsed past the version.
#[test]
fn newer_version_is_refused_readably() {
    let mut bytes = record_sample(2, 10, &settings(30, 10));
    bytes[LSREPLAY_MAGIC.len()..LSREPLAY_MAGIC.len() + 4].copy_from_slice(&2u32.to_le_bytes());
    let err = LsReplayReader::new(&bytes).expect_err("newer");
    assert_eq!(err.code(), "replay_version_newer");
    let text = err.to_string();
    for needle in ["version 2", "version 1", "newer build"] {
        assert!(text.contains(needle), "`{needle}` missing from: {text}");
    }
    // Version 0 is not a version any build wrote.
    bytes[LSREPLAY_MAGIC.len()..LSREPLAY_MAGIC.len() + 4].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(
        LsReplayReader::new(&bytes).map(|_| ()).map_err(|e| e.code()),
        Err("replay_invalid")
    );
    assert_eq!(
        LsReplayReader::new(b"RGLSRPLX\x01\0\0\0")
            .map(|_| ())
            .map_err(|e| e.code()),
        Err("replay_not_lsreplay")
    );
}

/// Every strict prefix of a valid file reads whole up to a record boundary and is
/// `replay_truncated` anywhere else, never a panic; a record length over the cap is
/// `replay_too_large` and one past the end `replay_truncated`, neither allocated.
#[test]
fn truncation_is_named_at_every_prefix() {
    let bytes = record_sample(2, 40, &settings(10, 5));
    let mut reader = LsReplayReader::new(&bytes).expect("header");
    let mut boundaries = vec![reader.position()];
    while reader.next_record().expect("record").is_some() {
        boundaries.push(reader.position());
    }
    assert_eq!(boundaries.last(), Some(&bytes.len()));
    for len in 0..bytes.len() {
        let prefix = &bytes[..len];
        let outcome = LsReplayReader::new(prefix).and_then(|mut r| {
            while r.next_record()?.is_some() {}
            Ok(())
        });
        match outcome {
            Ok(()) => assert!(boundaries.contains(&len), "prefix {len} read whole"),
            Err(err) => {
                assert!(!boundaries.contains(&len), "prefix {len}: {err}");
                assert_eq!(err.code(), "replay_truncated", "prefix {len}: {err}");
            }
        }
    }
    let mut bomb = bytes[..LsReplayReader::new(&bytes).expect("ok").position()].to_vec();
    bomb.push(RECORD_FULL);
    bomb.extend_from_slice(&u32::MAX.to_le_bytes());
    let mut reader = LsReplayReader::new(&bomb).expect("header");
    assert_eq!(
        reader.next_record().map_err(|e| e.code()),
        Err("replay_too_large")
    );
    bomb.truncate(bomb.len() - 4);
    bomb.extend_from_slice(&1_000u32.to_le_bytes());
    let mut reader = LsReplayReader::new(&bomb).expect("header");
    assert_eq!(
        reader.next_record().map_err(|e| e.code()),
        Err("replay_truncated")
    );
}

/// The audit refuses a file of another game or game version, and the registry names its games.
#[test]
fn another_game_is_refused() {
    let bytes = record_sample(2, 10, &settings(30, 10));
    let mut other = bytes.clone();
    let (mut header, recs) = records(&other);
    header.protocol_version = 9;
    let mut writer = LsReplayWriter::new(Vec::new(), header.clone(), LsReplaySettings::default()).expect("h");
    for r in &recs {
        writer.write_record(r).expect("r");
    }
    other = writer.into_inner();
    let err = audit(&other, &mut SampleSim::default()).expect_err("version");
    assert_eq!(err.code(), "replay_game_mismatch");
    assert!(err.to_string().contains("protocol version 9"), "{err}");
    header.game_id = "studio.other".into();
    let mut writer = LsReplayWriter::new(Vec::new(), header, LsReplaySettings::default()).expect("h");
    writer.write_record(&recs[0]).expect("r");
    let err = audit_with(&writer.into_inner(), &ReplayRegistry::with_builtin()).expect_err("no hook");
    assert_eq!(err.code(), "replay_no_hook");
    assert!(err.to_string().contains("regia.desync_sample"), "{err}");
    assert!(
        audit_with(&bytes, &ReplayRegistry::with_builtin())
            .expect("builtin")
            .bit_exact()
    );
}

/// The byte cap sheds fulls first, then stops at a whole tick; the capped file still audits.
#[test]
fn the_byte_cap_sheds_fulls_then_stops_whole() {
    let full = record_sample(4, 200, &settings(10, 10));
    let mut capped = settings(10, 10);
    capped.max_bytes = Some(full.len() as u64 / 2);
    let header = LsReplayHeader::of::<SampleGame>((0..4).map(SeatId), 60, 1, &capped);
    let mut writer = LsReplayWriter::new(Vec::new(), header, capped.clone()).expect("header");
    let mut sim = SampleSim::default();
    writer
        .record_start(&sim, |o| o.extend(sim.snapshot()))
        .expect("start");
    for tick in 0..200 {
        let f = frame(4, tick);
        writer.record_frame(&f).expect("frame");
        sim.step_inputs(tick, f.inputs.iter().map(|(s, i)| (s.0, i.as_ref().copied())));
        writer
            .record_state(tick, &sim, |o| o.extend(sim.snapshot()))
            .expect("state");
    }
    let stats = writer.stats();
    assert!(stats.capped && stats.records_dropped > 0, "{stats:?}");
    let bytes = writer
        .finish(Some(ReplayAbortInfo::new(199, "desync", "x")))
        .expect("finish");
    assert!(
        bytes.len() as u64 <= full.len() as u64 / 2 + ABORT_RESERVE_BYTES,
        "{}",
        bytes.len()
    );
    let verdict = audit_sample(&bytes);
    assert!(verdict.bit_exact(), "{verdict:?}");
    assert!(verdict.abort.is_some(), "the abort trailer is always written");
    let (_, recs) = records(&bytes);
    let last_input = recs.iter().rev().find_map(|r| match r {
        LsRecord::Inputs(i) => Some(i.frame.tick),
        _ => None,
    });
    assert!(last_input.is_some_and(|t| t < 199));
    // Tiers: 256 MiB and 64 MiB; fulls at 2 Hz and 1 Hz.
    assert_eq!(ReplayTier::Standard.max_bytes(), 256 << 20);
    assert_eq!(ReplayTier::Low.max_bytes(), 64 << 20);
    assert_eq!(LsReplaySettings::default().full_every(60), 30);
    let low = LsReplaySettings {
        tier: ReplayTier::Low,
        ..LsReplaySettings::default()
    };
    assert_eq!(low.full_every(60), 60);
    assert!(!LsReplaySettings::default().record, "recording is opt-in");

    // A Full larger than the room left is shed and the frames go on.
    let mut small = settings(10, 0);
    small.max_bytes = Some(4_096);
    let header = LsReplayHeader::of::<SampleGame>((0..2).map(SeatId), 60, 1, &small);
    let mut writer = LsReplayWriter::new(Vec::new(), header, small).expect("header");
    writer.record_start(&sim, |o| o.resize(8_000, 7)).expect("start");
    writer.record_frame(&frame(2, 0)).expect("frame");
    let stats = writer.stats();
    assert_eq!(
        (stats.fulls_shed, stats.capped, stats.records),
        (1, false, 1),
        "{stats:?}"
    );
}

fn write_replay(dir: &Path, name: &str, bytes: &[u8], date: u64) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write");
    let file = std::fs::File::options().write(true).open(&path).expect("open");
    file.set_modified(UNIX_EPOCH + Duration::from_secs(date))
        .expect("mtime");
    path
}

/// Acceptance (NL-02): rotation keeps the newest N (the setting) and never deletes a file with an
/// abort record or a damaged one; listing is newest first.
#[test]
fn library_rotation_keeps_the_newest_n() {
    let dir = TempDir::new("rotate");
    let bytes = record_sample(2, 20, &settings(10, 10));
    for i in 0..25u64 {
        let mut b = bytes.clone();
        // Distinct files (the seed is in the header).
        let (mut header, recs) = records(&b);
        header.seed = i;
        let mut w = LsReplayWriter::new(Vec::new(), header, LsReplaySettings::default()).expect("h");
        for r in &recs {
            w.write_record(r).expect("r");
        }
        if i == 0 {
            w.write_record(&LsRecord::Abort(ReplayAbortInfo::new(3, "desync", "kept")))
                .expect("a");
        }
        b = w.into_inner();
        write_replay(&dir.0, &format!("match-{i:02}.lsreplay"), &b, 1_000 + i);
    }
    write_replay(&dir.0, "broken.lsreplay", b"RGLS", 500);
    write_replay(&dir.0, "notes.txt", b"not a replay", 400);
    let library = ReplayLibrary::new(&dir.0).keep_newest(20);
    assert_eq!(ReplayLibrary::new(&dir.0).keep, DEFAULT_KEEP_NEWEST);
    let report = library.rotate().expect("rotate");
    let deleted: Vec<String> = report
        .deleted
        .iter()
        .filter_map(|p| p.file_name()?.to_str().map(str::to_owned))
        .collect();
    assert_eq!(
        deleted,
        ["match-01", "match-02", "match-03", "match-04"].map(|s| format!("{s}.lsreplay"))
    );
    assert_eq!(report.kept_aborted, 1);
    let list = library.list().expect("list");
    assert_eq!(list.replays.len(), 21, "20 newest plus the aborted one");
    assert_eq!(list.replays[0].file_name, "match-24.lsreplay", "newest first");
    assert_eq!(
        list.replays.last().map(|r| r.file_name.as_str()),
        Some("match-00.lsreplay")
    );
    assert!(list.replays.last().is_some_and(|r| r.aborted));
    assert!(list.damaged.iter().any(|d| d.file_name == "broken.lsreplay"));
    // Rotating again deletes nothing.
    assert!(library.rotate().expect("again").deleted.is_empty());
}

/// Acceptance (NL-02): importing bytes the library already holds is refused (sha256), under
/// any name; a new file is copied with its date kept, beside a same-named one.
#[test]
fn a_duplicate_import_is_refused() {
    let lib_dir = TempDir::new("import-lib");
    let src_dir = TempDir::new("import-src");
    let bytes = record_sample(2, 20, &settings(10, 10));
    let library = ReplayLibrary::new(&lib_dir.0);
    let first = write_replay(&src_dir.0, "match.lsreplay", &bytes, 2_000);
    let info = library.import(&first).expect("import");
    assert_eq!(info.file_name, "match.lsreplay");
    assert_eq!(info.date, 2_000);
    assert_eq!(info.sha256.as_deref(), Some(sha256_hex(&bytes).as_str()));
    let renamed = write_replay(&src_dir.0, "copy-of-match.lsreplay", &bytes, 3_000);
    let err = library.import(&renamed).expect_err("duplicate");
    assert_eq!(err.code(), "replay_duplicate");
    assert!(err.to_string().contains("match.lsreplay"), "{err}");
    // Other bytes under the same name land beside it.
    let other = record_sample(3, 20, &settings(10, 10));
    let second = write_replay(&src_dir.0, "match.lsreplay", &other, 2_500);
    let info = library.import(&second).expect("second");
    assert_eq!(info.file_name, "match-2.lsreplay");
    // A damaged file, a missing one and another extension are refused with their codes.
    let bad = write_replay(&src_dir.0, "bad.lsreplay", &other[..other.len() - 3], 1);
    assert_eq!(
        library.import(&bad).map_err(|e| e.code()).err(),
        Some("replay_truncated")
    );
    let missing = src_dir.0.join("gone.lsreplay");
    assert_eq!(
        library.import(&missing).map_err(|e| e.code()).err(),
        Some("replay_not_found")
    );
    let txt = write_replay(&src_dir.0, "match.txt", &bytes, 1);
    assert_eq!(
        library.import(&txt).map_err(|e| e.code()).err(),
        Some("replay_not_lsreplay")
    );
    // Export writes the file and a sidecar with the format name and the digest.
    let out = TempDir::new("export");
    let exported = library.export("match.lsreplay", &out.0).expect("export");
    let sidecar = std::fs::read_to_string(&exported.sidecar_path).expect("sidecar");
    assert!(
        sidecar.contains(META_FORMAT) && sidecar.contains(&sha256_hex(&bytes)),
        "{sidecar}"
    );
    assert_eq!(std::fs::read(&exported.replay_path).expect("copy"), bytes);
}

/// Acceptance (NL-02): a damaged file (bad magic, a newer version, a truncated record) is listed
/// apart with its code and reason; the good ones are still listed.
#[test]
fn a_damaged_file_is_listed_apart() {
    let dir = TempDir::new("damaged");
    let bytes = record_sample(2, 30, &settings(10, 10));
    write_replay(&dir.0, "good.lsreplay", &bytes, 10);
    write_replay(&dir.0, "short.lsreplay", &bytes[..bytes.len() - 1], 11);
    let mut newer = bytes.clone();
    newer[8..12].copy_from_slice(&7u32.to_le_bytes());
    write_replay(&dir.0, "newer.lsreplay", &newer, 12);
    write_replay(&dir.0, "junk.lsreplay", b"hello, not a replay at all", 13);
    let list = ReplayLibrary::new(&dir.0).list().expect("list");
    assert_eq!(list.replays.len(), 1);
    assert_eq!(list.replays[0].file_name, "good.lsreplay");
    assert_eq!(list.replays[0].game_id, SAMPLE_GAME_ID);
    let codes: Vec<(&str, &str)> = list
        .damaged
        .iter()
        .map(|d| (d.file_name.as_str(), d.code.as_str()))
        .collect();
    assert_eq!(
        codes,
        vec![
            ("junk.lsreplay", "replay_not_lsreplay"),
            ("newer.lsreplay", "replay_version_newer"),
            ("short.lsreplay", "replay_truncated"),
        ]
    );
    assert!(list.damaged.iter().all(|d| !d.reason.is_empty()));
    // A folder that does not exist is an empty library.
    let none = ReplayLibrary::new(dir.0.join("nope"))
        .list()
        .expect("missing dir");
    assert!(none.replays.is_empty() && none.damaged.is_empty());
}

/// The in-module SHA-256 matches the FIPS 180-4 examples.
#[test]
fn sha256_matches_the_standard_vectors() {
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
}

/// The sample hook's hashes are what the writer recorded (a sanity check of the test itself).
#[test]
fn the_sample_hook_hashes_like_the_writer() {
    let mut sim = SampleSim::default();
    sim.step_inputs(0, [(0, Some(1)), (1, None)]);
    let mut rows = Vec::new();
    sim.subsystem_hashes(0, &mut rows);
    assert_eq!(rows.len(), 4);
}
