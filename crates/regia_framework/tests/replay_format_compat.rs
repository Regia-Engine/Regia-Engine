// SPDX-License-Identifier: MPL-2.0
//! `.do27replay` compatibility against frozen, checked-in fixtures (ADR-1378).
//!
//! The unit tests in `replay_sim.rs` read bytes made by [`SyntheticReplay`], the writer in the same
//! module, so a change made to both the reader and the writer would keep them green while every file
//! DO27 has already written stops loading. The files in `tests/fixtures/replay/` were written once, from
//! the byte layout, by a second encoder that shares no code with `SyntheticReplay`; they are frozen.
//!
//! - `v3_every_record.do27replay`: a version 3 file with one or more of every record tag (`0x01`..`0x07`),
//!   a non-ASCII callsign, extreme input values, an empty full, a Moment kind the reader does not name and
//!   a 32-byte weapon stem.
//! - `v2_no_fulls.do27replay`: a version 2 file (short header, no fulls) with inputs, a command, a
//!   Moment, a salvage ledger and an empty nav record.
//! - `v4_future.do27replay`: a v3 layout stamped v4, retained as malformed (ADR-8057).
//! - `v3_unknown_tag.do27replay`: a valid version 3 prefix, then a record tag (`0x08`) this reader
//!   does not know. Tags are never skipped.
//! - `v4_extensions.do27replay`: a genuine v4 header and the v3 records with opaque map/training
//!   extensions before, between and after them. All original fixtures remain byte-identical.
//!
//! **Never regenerate a fixture.** A new layout (a version 4, a new tag) adds a new fixture and keeps the
//! old ones passing; [`FIXTURES`] pins each file's length and FNV-1a hash so an edited fixture fails here
//! first.

use std::path::PathBuf;

use regia_core::math::Vec3;
use regia_framework::replay_sim::{
    AbortRecord, AppliedInputs, FullRecord, Moment, MomentKind, NavBeacon, NavRecord, NavWaypoint,
    Participant, PeerCommand, PeerCommands, PeerRow, PilotInput, PlayerRow, ReplayError, ReplayEvent,
    ReplayFile, ReplayHeader, ReplaySim, SalvageLedger, SyntheticReplay,
};

/// Every fixture: file name, byte length, FNV-1a 64 of the bytes. Frozen with the files.
const FIXTURES: [(&str, usize, u64); 5] = [
    ("v3_every_record.do27replay", 699, 0x516f_c85a_2cf2_7110),
    ("v2_no_fulls.do27replay", 264, 0xdf46_c59e_e073_596b),
    ("v4_future.do27replay", 153, 0x9b51_96b7_fd2f_6feb),
    ("v3_unknown_tag.do27replay", 153, 0x4947_751a_6d28_2c1e),
    ("v4_extensions.do27replay", 801, 0xc804_0395_3397_0aa3),
];

/// Byte offset of the `0x08` tag in `v3_unknown_tag.do27replay` (a 142-byte header, then a 6-byte
/// empty inputs record).
const UNKNOWN_TAG_OFFSET: usize = 148;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/replay")
        .join(name)
}

fn fixture(name: &str) -> Vec<u8> {
    let path = fixture_path(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn pilot(throttle: i8, strafe: i8, steer: i8, aim_pitch: i16, action_bits: u16, stance: u8) -> PilotInput {
    PilotInput {
        throttle,
        strafe,
        steer,
        aim_pitch,
        action_bits,
        stance,
    }
}

/// One Moment. `(drained_tick, kind)`, the three spans, the two participant slots, the position,
/// `(damage, importance)`, the weapon stem and `(attacker, victim)`.
fn moment(
    (drained_tick, kind): (u32, MomentKind),
    spans: [u32; 3],
    participants: [(u8, u32); 2],
    position: Vec3,
    (damage, importance): (u32, u8),
    weapon: &str,
    (attacker, victim): (u32, u32),
) -> Moment {
    Moment {
        drained_tick,
        tick: u64::from(drained_tick),
        kind,
        span_match: spans[0],
        span_round: spans[1],
        span_fight: spans[2],
        participants: participants.map(|(role, entity_id)| Participant { role, entity_id }),
        position,
        damage,
        weapon: weapon.to_owned(),
        attacker,
        victim,
        importance,
    }
}

// ---------------------------------------------------------------------------
// The version 3 fixture, field by field
// ---------------------------------------------------------------------------

fn v3_header() -> ReplayHeader {
    ReplayHeader {
        version: 3,
        protocol: "do27/3".into(),
        tick_rate: 30,
        lobby_preset: "ranked".into(),
        seed: 0x0123_4567_89AB_CDEF,
        bootstrap: "bg_fixture".into(),
        map: "fixture_map".into(),
        mode: "conquest".into(),
        teams: 2,
        players: vec![
            PlayerRow {
                peer_id: 3,
                account_id: "a1b2".into(),
                callsign: "Wren".into(),
                role: 1,
                team: 0,
            },
            PlayerRow {
                peer_id: 5,
                account_id: String::new(),
                callsign: "Ödön".into(),
                role: 0,
                team: 1,
            },
        ],
        input_delay: 4,
        peers: vec![
            PeerRow { peer_id: 3, role: 1 },
            PeerRow { peer_id: 5, role: 0 },
            PeerRow { peer_id: 9, role: 3 },
        ],
        sim_git_sha: "0123abcd".into(),
        sim_abi: 12,
        content_hash: "feedface".into(),
        config_hash: String::new(),
    }
}

fn v3_inputs() -> Vec<AppliedInputs> {
    vec![
        AppliedInputs {
            tick: 0,
            inputs: vec![
                (3, pilot(127, -128, 0, -32768, 0xFFFF, 2)),
                (5, pilot(-127, 1, -1, 32767, 0x0001, 0)),
            ],
        },
        AppliedInputs {
            tick: 1,
            inputs: vec![],
        },
        AppliedInputs {
            tick: 2,
            inputs: vec![(3, PilotInput::default())],
        },
    ]
}

fn v3_salvage() -> SalvageLedger {
    SalvageLedger {
        tick: 0,
        rows: [
            [1, 2, 3, 4, 5],
            [6, 7, 8, 9, 10],
            [11, 12, 13, 14, 15],
            [16, 17, 18, 19, 20],
        ],
    }
}

fn v3_nav() -> NavRecord {
    NavRecord {
        tick: 0,
        beacons: vec![
            NavBeacon {
                id: 0x0102,
                owner_slot: 1,
                team: 0,
                number: 7,
                x_cm: 123_456,
                y_cm: -50,
                z_cm: -987_654,
                health: 1000,
            },
            NavBeacon {
                id: 2,
                owner_slot: 2,
                team: 1,
                number: 1,
                x_cm: 0,
                y_cm: 0,
                z_cm: 0,
                health: 0,
            },
        ],
        waypoints: vec![
            NavWaypoint {
                team: 0,
                slot: 1,
                id: 0x0102,
            },
            NavWaypoint {
                team: 1,
                slot: 0,
                id: 2,
            },
        ],
    }
}

fn v3_commands() -> PeerCommands {
    PeerCommands {
        tick: 0,
        commands: vec![
            PeerCommand {
                peer_id: 3,
                ok: true,
                kind: "build".into(),
                text: "depot at 12,-4".into(),
            },
            PeerCommand {
                peer_id: 5,
                ok: false,
                kind: "order".into(),
                text: String::new(),
            },
        ],
    }
}

fn v3_moments() -> Vec<Moment> {
    vec![
        moment(
            (1, MomentKind::Hit),
            [1, 1, 1],
            [(1, 100), (2, 200)],
            Vec3::new(1.5, -2.25, 1000.0),
            (75, 90),
            "autocannon",
            (100, 200),
        ),
        moment(
            (1, MomentKind::Down),
            [1, 1, 1],
            [(0, 0), (2, 201)],
            Vec3::ZERO,
            (0, 0),
            "",
            (0, 201),
        ),
        moment(
            (2, MomentKind::Other(9)),
            [1, 2, 3],
            [(1, 7), (2, 8)],
            Vec3::new(-1.0, 2.0, -3.0),
            (1, 100),
            "abcdefghijklmnopqrstuvwxyz012345",
            (7, 8),
        ),
    ]
}

const V3_SNAPSHOT_0: [u8; 5] = [0x10, 0x20, 0x30, 0x40, 0x50];
const V3_HASH_0: u64 = 0xDEAD_BEEF_CAFE_F00D;

/// The version 3 fixture's content through `SyntheticReplay`, record for record in file order.
fn v3_synthetic() -> Vec<u8> {
    let inputs = v3_inputs();
    let moments = v3_moments();
    SyntheticReplay::new(&v3_header())
        .inputs(0, &inputs[0].inputs)
        .full(&V3_SNAPSHOT_0, V3_HASH_0)
        .salvage(&v3_salvage())
        .nav(&v3_nav())
        .commands(&v3_commands())
        .inputs(1, &inputs[1].inputs)
        .moments(1, &moments[..2])
        .inputs(2, &inputs[2].inputs)
        .full(&[], 0)
        .moments(2, &moments[2..])
        .abort(2, "host left")
        .finish()
}

// ---------------------------------------------------------------------------
// The version 2 fixture
// ---------------------------------------------------------------------------

fn v2_header() -> ReplayHeader {
    ReplayHeader {
        version: 2,
        protocol: "do27/2".into(),
        tick_rate: 60,
        lobby_preset: "alpha".into(),
        ..ReplayHeader::default()
    }
}

fn v2_moment() -> Moment {
    moment(
        (1, MomentKind::Kill),
        [1, 1, 1],
        [(1, 4), (2, 5)],
        Vec3::new(10.0, 0.0, -10.0),
        (250, 60),
        "rail",
        (4, 5),
    )
}

fn v2_commands() -> PeerCommands {
    PeerCommands {
        tick: 0,
        commands: vec![PeerCommand {
            peer_id: 1,
            ok: true,
            kind: "place".into(),
            text: "turret".into(),
        }],
    }
}

fn v2_synthetic() -> Vec<u8> {
    SyntheticReplay::new(&v2_header())
        .inputs(0, &[(1, pilot(10, 0, 0, 0, 0, 0))])
        .commands(&v2_commands())
        .inputs(1, &[])
        .moments(1, &[v2_moment()])
        .salvage(&SalvageLedger {
            tick: 1,
            rows: [[0; 5]; 4],
        })
        .nav(&NavRecord {
            tick: 1,
            beacons: vec![],
            waypoints: vec![],
        })
        .finish()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn fixtures_are_frozen() {
    for (name, len, hash) in FIXTURES {
        let bytes = fixture(name);
        assert_eq!(
            bytes.len(),
            len,
            "{name}: length changed; fixtures are frozen (ADR-1378)"
        );
        assert_eq!(
            fnv1a(&bytes),
            hash,
            "{name}: content changed; fixtures are frozen, add a new one instead (ADR-1378)"
        );
    }
}

#[test]
fn v3_fixture_parses_every_record_field_by_field() {
    let file = ReplayFile::parse(&fixture("v3_every_record.do27replay")).expect("v3 fixture parses");
    assert_eq!(file.header, v3_header());
    assert_eq!(file.inputs, v3_inputs());
    assert_eq!(
        file.fulls,
        vec![
            FullRecord {
                tick: 0,
                snapshot: V3_SNAPSHOT_0.to_vec(),
                state_hash: Some(V3_HASH_0),
            },
            FullRecord {
                tick: 2,
                snapshot: vec![],
                state_hash: Some(0),
            },
        ]
    );
    assert_eq!(file.salvage, vec![v3_salvage()]);
    assert_eq!(file.navs, vec![v3_nav()]);
    assert_eq!(file.commands, vec![v3_commands()]);
    assert_eq!(file.moments, v3_moments());
    assert_eq!(file.moment_batches, 2);
    assert_eq!(
        file.abort,
        Some(AbortRecord {
            tick: 2,
            reason: "host left".into(),
        })
    );
    assert_eq!(file.byte_len, 699);
    assert_eq!(file.end_tick(), 2);
}

#[test]
fn v3_fixture_details_that_a_layout_slip_would_move() {
    let file = ReplayFile::parse(&fixture("v3_every_record.do27replay")).expect("v3 fixture parses");
    // Signed extremes: a sign or width slip in the input row shows here.
    assert_eq!(file.inputs[0].inputs[0].1.aim_pitch, i16::MIN);
    assert_eq!(file.inputs[0].inputs[1].1.aim_pitch, i16::MAX);
    // The Moment kind the reader does not name keeps its wire value.
    assert_eq!(file.moments[2].kind.to_wire(), 9);
    assert_eq!(file.moments[2].kind.name(), "other");
    // A weapon stem exactly 32 bytes long fills the whole field.
    assert_eq!(file.moments[2].weapon.len(), 32);
    // A nav position is centimetres in the file and metres in Regia Engine.
    assert_eq!(
        file.navs[0].beacons[0].position(),
        Vec3::new(1234.56, -0.5, -9876.54)
    );
    // UTF-8 header strings survive (the length prefix counts bytes, not characters).
    assert_eq!(file.header.players[1].callsign.chars().count(), 4);
    assert_eq!(file.header.players[1].callsign.len(), 6);
}

#[test]
fn synthetic_writer_matches_the_v3_fixture_byte_for_byte() {
    let fixture = fixture("v3_every_record.do27replay");
    let written = v3_synthetic();
    let first_diff = fixture.iter().zip(&written).position(|(a, b)| a != b);
    assert_eq!(
        (written.len(), first_diff),
        (fixture.len(), None),
        "SyntheticReplay no longer writes the frozen layout; every unit test built on it is suspect"
    );
}

#[test]
fn v4_file_with_unknown_record_plays() {
    let bytes = fixture("v4_extensions.do27replay");
    let mut sim = ReplaySim::new();
    sim.load_bytes(&bytes)
        .expect("v4 with opaque extension records loads");
    let parsed = ReplayFile::parse(&bytes).expect("v4 parses");
    let mut expected = ReplayFile::parse(&fixture("v3_every_record.do27replay")).expect("v3");
    expected.header.version = 4;
    expected.byte_len = bytes.len();
    assert_eq!(
        parsed, expected,
        "extensions cannot change known records or their ticks"
    );
    assert_eq!(sim.end_tick(), 2, "extension ticks do not extend playback");
    assert_eq!(sim.step(1), 1);
    assert_eq!(sim.last_step_events().len(), 4);
    assert_eq!(sim.step(1), 1);
    assert_eq!(sim.last_step_events().len(), 2);
    assert_eq!(sim.step(1), 0);
    assert!(sim.last_step_events().is_empty());
}

#[test]
fn v2_fixture_parses_with_empty_version_3_fields() {
    let file = ReplayFile::parse(&fixture("v2_no_fulls.do27replay")).expect("v2 fixture parses");
    assert_eq!(file.header, v2_header());
    assert!(file.header.players.is_empty() && file.header.peers.is_empty());
    assert_eq!(file.header.sim_abi, 0);
    assert!(file.fulls.is_empty());
    assert_eq!(
        file.inputs,
        vec![
            AppliedInputs {
                tick: 0,
                inputs: vec![(1, pilot(10, 0, 0, 0, 0, 0))],
            },
            AppliedInputs {
                tick: 1,
                inputs: vec![],
            },
        ]
    );
    assert_eq!(file.commands, vec![v2_commands()]);
    assert_eq!(file.moments, vec![v2_moment()]);
    assert_eq!(file.salvage.len(), 1);
    assert_eq!(file.salvage[0].rows, [[0; 5]; 4]);
    assert_eq!(file.navs.len(), 1);
    assert!(file.navs[0].beacons.is_empty() && file.navs[0].waypoints.is_empty());
    assert!(file.abort.is_none());
    assert_eq!(file.end_tick(), 1);
    // A version 2 file has no hashes to check: it is never reported as verified.
    let report = file.verify(&regia_framework::replay_sim::OpaqueCodec);
    assert_eq!((report.fulls, report.hash_checked), (0, 0));
    assert!(report.defects.iter().any(|d| d == "no full record"), "{report:?}");
}

#[test]
fn synthetic_writer_matches_the_v2_fixture_byte_for_byte() {
    assert_eq!(v2_synthetic(), fixture("v2_no_fulls.do27replay"));
}

#[test]
fn a_future_version_is_refused_at_the_version_field() {
    let mut bytes = fixture("v4_extensions.do27replay");
    bytes[4..8].copy_from_slice(&5_u32.to_le_bytes());
    let err = ReplayFile::parse(&bytes).expect_err("version 5 must not parse");
    assert_eq!(err, ReplayError::UnsupportedVersion(5));
    assert_eq!(err.code(), "unsupported_version");
    assert!(err.to_string().contains("accepts 2, 3 and 4"), "{err}");
}

#[test]
fn the_old_future_fixture_is_still_rejected_as_malformed() {
    assert_eq!(
        ReplayFile::parse(&fixture("v4_future.do27replay")),
        Err(ReplayError::Truncated {
            offset: 98,
            needed: 46_732
        })
    );
}

#[test]
fn an_unknown_tag_is_refused_where_it_starts() {
    let err = ReplayFile::parse(&fixture("v3_unknown_tag.do27replay")).expect_err("tag 0x08 must not parse");
    assert_eq!(
        err,
        ReplayError::UnknownTag {
            tag: 0x08,
            offset: UNKNOWN_TAG_OFFSET,
        }
    );
    assert_eq!(err.code(), "unknown_tag");
    // Everything before the unknown tag is a valid file on its own.
    let prefix = fixture("v3_unknown_tag.do27replay");
    let head = ReplayFile::parse(&prefix[..UNKNOWN_TAG_OFFSET]).expect("the prefix parses");
    assert_eq!(head.header, v3_header());
    assert_eq!(head.inputs.len(), 1);
}

#[test]
fn load_from_disk_equals_parse_from_memory() {
    for name in [
        "v3_every_record.do27replay",
        "v2_no_fulls.do27replay",
        "v4_extensions.do27replay",
    ] {
        let loaded = ReplayFile::load(&fixture_path(name)).expect("load");
        let parsed = ReplayFile::parse(&fixture(name)).expect("parse");
        assert_eq!(loaded, parsed, "{name}");
    }
    assert_eq!(
        ReplayFile::load(&fixture_path("v4_future.do27replay")),
        Err(ReplayError::Truncated {
            offset: 98,
            needed: 46_732
        })
    );
}

#[test]
fn every_prefix_of_every_fixture_is_an_error_or_a_shorter_file() {
    for (name, ..) in FIXTURES {
        let bytes = fixture(name);
        // The header a valid prefix must carry; `None` where no prefix may parse at all.
        let header = match name {
            "v2_no_fulls.do27replay" => Some(v2_header()),
            "v4_future.do27replay" => None,
            "v4_extensions.do27replay" => Some(ReplayHeader {
                version: 4,
                ..v3_header()
            }),
            _ => Some(v3_header()),
        };
        let mut ok_cuts = 0;
        for cut in 0..bytes.len() {
            // Never a panic; a cut on a record boundary is a valid, shorter file of the same header.
            if let Ok(file) = ReplayFile::parse(&bytes[..cut]) {
                assert_eq!(Some(&file.header), header.as_ref(), "{name} cut at {cut}");
                ok_cuts += 1;
            }
        }
        assert_eq!(ok_cuts > 0, header.is_some(), "{name}: {ok_cuts} valid prefixes");
    }
}

#[test]
fn playback_of_the_v3_fixture_fires_each_event_once_in_order() {
    let mut sim = ReplaySim::new();
    sim.load_bytes(&fixture("v3_every_record.do27replay"))
        .expect("load");
    assert_eq!(sim.end_tick(), 2);
    let mut seen = Vec::new();
    for _ in 0..4 {
        sim.step(1);
        for event in sim.last_step_events() {
            seen.push(match event {
                ReplayEvent::Moment(m) => format!("{}:moment.{}", m.drained_tick, m.kind.name()),
                ReplayEvent::Command { tick, command } => format!("{tick}:command.{}", command.kind),
                ReplayEvent::Abort(a) => format!("{}:abort", a.tick),
            });
        }
    }
    assert_eq!(
        seen,
        [
            "0:command.build",
            "0:command.order",
            "1:moment.hit",
            "1:moment.down",
            "2:moment.other",
            "2:abort",
        ]
    );
    assert_eq!(sim.tick(), 2);
}

#[test]
fn snapshots_of_the_v3_fixture_without_a_codec_report_beacons_and_the_missing_codec() {
    let mut sim = ReplaySim::new();
    sim.load_bytes(&fixture("v3_every_record.do27replay"))
        .expect("load");
    let snap = sim.snapshot();
    assert_eq!((snap.tick, snap.full_tick, snap.nav_tick), (0, Some(0), Some(0)));
    assert_eq!(snap.entities.len(), 2, "two beacons, no codec entities");
    assert!(snap.entities.iter().all(|e| e.kind.as_ref() == "beacon"));
    assert!(
        snap.decode_error
            .as_deref()
            .is_some_and(|e| e.starts_with("no_full_codec")),
        "{snap:?}"
    );
    sim.seek(2);
    let snap = sim.snapshot();
    assert_eq!((snap.full_tick, snap.nav_tick), (Some(2), Some(0)));
    // Without a hashing codec both fulls are unchecked, never passed.
    let report = sim.verify().expect("loaded");
    assert_eq!(
        (report.fulls, report.hash_checked, report.hash_unchecked),
        (2, 0, 2)
    );
    assert!(report.is_ok(), "{report:?}");
    assert!(report.aborted);
}

/// The independent v4 fixture has two extra player strings and two team strings.
const V4_HEADER_BYTES: usize = 197;

#[test]
fn v4_extensions_reject_every_partial_envelope_and_body() {
    let header = fixture("v4_extensions.do27replay")[..V4_HEADER_BYTES].to_vec();
    let records = [
        [vec![0x08], vec![0x02; 16]].concat(),
        [
            vec![0x09],
            123_u32.to_le_bytes().to_vec(),
            3_u32.to_le_bytes().to_vec(),
            vec![1, 2, 7],
        ]
        .concat(),
    ];
    for record in records {
        for cut in 1..record.len() {
            let bytes = [&header[..], &record[..cut]].concat();
            assert!(
                matches!(ReplayFile::parse(&bytes), Err(ReplayError::Truncated { .. })),
                "tag {} at cut {cut}",
                record[0]
            );
        }
        assert!(ReplayFile::parse(&[&header[..], &record[..]].concat()).is_ok());
    }
    let oversized = [
        header,
        vec![0x09],
        0_u32.to_le_bytes().to_vec(),
        u32::MAX.to_le_bytes().to_vec(),
    ]
    .concat();
    assert_eq!(
        ReplayFile::parse(&oversized),
        Err(ReplayError::Truncated {
            offset: V4_HEADER_BYTES + 9,
            needed: u32::MAX as usize,
        })
    );
}

#[test]
fn v4_unknown_boundaries_are_never_guessed() {
    let header = fixture("v4_extensions.do27replay")[..V4_HEADER_BYTES].to_vec();
    for tag in (0..=u8::MAX).filter(|tag| !(1..=9).contains(tag)) {
        let bytes = [&header[..], &[tag, 0, 0, 0, 0, 0x02, 0, 0, 0, 0, 0]].concat();
        assert_eq!(
            ReplayFile::parse(&bytes),
            Err(ReplayError::UnknownTag {
                tag,
                offset: V4_HEADER_BYTES,
            })
        );
    }
}

#[test]
fn v4_header_labels_are_validated_even_though_not_exposed() {
    let original = fixture("v4_extensions.do27replay");
    let label_offsets: Vec<_> = original[..V4_HEADER_BYTES]
        .windows(5)
        .enumerate()
        .filter(|(_, bytes)| *bytes == b"group" || *bytes == "grüp".as_bytes())
        .map(|(i, _)| i)
        .collect();
    assert_eq!(label_offsets.len(), 4);
    for at in label_offsets {
        let mut bytes = original.clone();
        bytes[at] = 0xff;
        assert_eq!(
            ReplayFile::parse(&bytes),
            Err(ReplayError::BadString { offset: at - 2 })
        );
    }
}

#[test]
fn v4_skipped_tick_never_supplies_a_full_snapshot_tick() {
    let mut bytes = fixture("v4_extensions.do27replay")[..V4_HEADER_BYTES].to_vec();
    bytes.extend_from_slice(&[0x09, 1, 0, 0, 0, 0, 0, 0, 0]);
    let offset = bytes.len();
    bytes.extend_from_slice(&[0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(
        ReplayFile::parse(&bytes),
        Err(ReplayError::FullWithoutTick(offset))
    );
}

#[test]
fn legacy_versions_still_refuse_v4_extension_tags() {
    for version in [2, 3] {
        for tag in [0x08, 0x09] {
            let mut bytes = SyntheticReplay::new(&ReplayHeader {
                version,
                ..v3_header()
            })
            .finish();
            let offset = bytes.len();
            bytes.push(tag);
            bytes.extend_from_slice(&[0; 16]);
            assert_eq!(
                ReplayFile::parse(&bytes),
                Err(ReplayError::UnknownTag { tag, offset })
            );
        }
    }
}

#[test]
fn v4_codec_hashes_and_seek_preserve_full_snapshot_ticks() {
    use regia_core::math::Quat;
    use regia_framework::replay_sim::{FullCodec, ReplayEntity};

    struct ByteCodec;
    impl FullCodec for ByteCodec {
        fn decode(&self, snapshot: &[u8]) -> Result<Vec<ReplayEntity>, String> {
            let [x] = snapshot else {
                return Err("expected one position byte".into());
            };
            Ok(vec![ReplayEntity {
                id: 7,
                kind: "marker".into(),
                team: 0,
                translation: Vec3::new(f32::from(*x), 0.0, 0.0),
                rotation: Quat::IDENTITY,
            }])
        }
        fn state_hash(&self, snapshot: &[u8]) -> Option<u64> {
            Some(fnv1a(snapshot))
        }
    }

    let v3_header_len = SyntheticReplay::new(&v3_header()).finish().len();
    for bad_hash in [false, true] {
        let mut writer = SyntheticReplay::new(&v3_header());
        for tick in 0..=60 {
            writer = writer.inputs(tick, &[]);
            if tick == 10 {
                writer = writer.raw(&[0x08; 17]);
            }
            if tick == 20 {
                // Its tick must never replace the applied-input tick for a later full.
                writer = writer.raw(&[0x09, 0xff, 0xff, 0xff, 0xff, 1, 0, 0, 0, 0xff]);
            }
            if tick.is_multiple_of(30) {
                let snapshot = [tick as u8];
                let hash = fnv1a(&snapshot) ^ u64::from(bad_hash && tick == 30);
                writer = writer.full(&snapshot, hash);
            }
        }
        let records = writer.finish();
        let mut bytes = fixture("v4_extensions.do27replay")[..V4_HEADER_BYTES].to_vec();
        bytes.extend_from_slice(&records[v3_header_len..]);
        let mut sim = ReplaySim::with_codec(ByteCodec);
        sim.load_bytes(&bytes).expect("v4 playback with a matching codec");
        let report = sim.verify().expect("loaded");
        assert_eq!(
            (report.fulls, report.hash_checked, report.hash_unchecked),
            (3, 3, 0)
        );
        assert_eq!(report.is_ok(), !bad_hash);
        assert_eq!(report.mismatches, if bad_hash { vec![30] } else { vec![] });
        assert_eq!(sim.end_tick(), 60);
        for tick in [0, 1, 29, 30, 31, 59, 60, 20, 0] {
            sim.seek(tick);
            let snapshot = sim.snapshot();
            let full_tick = tick / 30 * 30;
            assert_eq!(snapshot.full_tick, Some(full_tick));
            assert_eq!(snapshot.entities[0].translation.x, full_tick as f32);
            assert!(snapshot.decode_error.is_none());
        }
    }
}
