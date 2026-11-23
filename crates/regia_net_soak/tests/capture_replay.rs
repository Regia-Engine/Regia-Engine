// SPDX-License-Identifier: MPL-2.0
//! Client capture and replay (spec 120 § 15.5, spec 103 § 14, ADR-8221): a bad-network failure on
//! a client replays to the same corrections.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use regia_net::capture::{CaptureFile, CaptureRecord, CaptureTap};
use regia_net_soak::profiles::Profile;
use regia_net_soak::workload::{self, Scenario, ServerMode};
use regia_net_soak::{REPLAY_ENV, replay_command, write_failure_captures};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A fresh folder under the temp dir (pid + counter), removed by the caller.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "regia-capture-replay-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn bad_wifi(clients: usize, seconds: u64, seed: u64) -> Scenario {
    Scenario {
        profile: Profile::named("bad-wifi").expect("bad-wifi"),
        clients,
        mode: ServerMode::Dedicated,
        seed,
        duration_ms: seconds * 1000,
        wire: Default::default(),
    }
}

/// Runs `s` with a capture on every client and returns the run and each client's saved file.
fn captured_run(s: &Scenario, ring: usize, tag: &str) -> (workload::RunRecord, Vec<CaptureFile>) {
    let taps: Vec<CaptureTap> = (0..s.clients)
        .map(|_| CaptureTap::new(ring).expect("tap"))
        .collect();
    let record = workload::run_with_captures(s, &taps).expect("run");
    let dir = temp_dir(tag);
    let files = taps
        .iter()
        .enumerate()
        .map(|(i, tap)| {
            let path = dir.join(format!("client-{i}.netcapture"));
            tap.save(&path, &workload::capture_label(s, i), false)
                .expect("saved");
            CaptureFile::load(&path).expect("loads")
        })
        .collect();
    let _ = std::fs::remove_dir_all(&dir);
    (record, files)
}

/// Acceptance 1: a seeded bad-wifi soak run's client capture, saved and loaded, replays to the
/// same correction count and the same output digest at every frame, with the same input stamps,
/// receive and flush times; and the run is the same with captures on as off.
#[test]
fn a_bad_wifi_capture_replays_to_the_same_corrections_and_digest() {
    let s = bad_wifi(4, 20, 0x5EED_0042);
    let plain = workload::run(&s).expect("run without captures");
    let (record, files) = captured_run(&s, 16 << 20, "accept1");
    for (a, b) in plain.clients.iter().zip(&record.clients) {
        assert_eq!(a, b, "a capture does not change the run");
    }
    let mut corrections = 0;
    for (i, file) in files.iter().enumerate() {
        let (scenario, client) = workload::capture_scenario(file).expect("label");
        assert_eq!((&scenario, client), (&s, i));
        let r = workload::replay_client(&scenario, client, file).expect("replay");
        let live = &record.clients[i];
        println!(
            "client {i}: {} frames, {} sessions, corrections {} of {} (live {} of {}), inputs {}, digest {:016x}",
            r.frames,
            r.sessions,
            r.record.corrections,
            r.record.corrections_checked,
            live.corrections,
            live.corrections_checked,
            r.record.inputs_sent,
            r.digest
        );
        assert_eq!(r.divergence, None, "client {i} replays record for record");
        assert_eq!(Some(r.digest), r.captured_digest, "client {i} output digest");
        assert_eq!(r.record.corrections, live.corrections, "client {i} corrections");
        assert_eq!(r.record.corrections_checked, live.corrections_checked);
        assert_eq!(r.record.correction_steps, live.correction_steps);
        assert_eq!(r.record.inputs_sent, live.inputs_sent, "client {i} input stamps");
        assert_eq!(r.record.snapshot_parts, live.snapshot_parts);
        assert!(r.frames > 1_000, "client {i} replayed its whole run");
        corrections += live.corrections;
    }
    assert!(
        corrections > 0,
        "bad-wifi makes the clients correct (the replay has something to match)"
    );
}

/// Acceptance 2: one changed byte in one captured datagram (a decrypted packet half way through a
/// session) changes the replay's output digest, and the comparison names where.
#[test]
fn a_one_byte_change_in_a_captured_datagram_changes_the_digest() {
    let s = bad_wifi(2, 12, 0x5EED_0043);
    let (_, files) = captured_run(&s, 16 << 20, "accept2");
    let file = &files[0];
    let clean = workload::replay_client(&s, 0, file).expect("replay");
    assert!(clean.matches(), "the unchanged capture replays exactly");
    let big: Vec<usize> = file
        .records
        .iter()
        .enumerate()
        .filter(|(_, r)| matches!(r, CaptureRecord::Packet { bytes } if bytes.len() > 200))
        .map(|(i, _)| i)
        .collect();
    assert!(big.len() > 10, "the session carries snapshot packets");
    let at = big[big.len() / 2];
    let mut changed = file.clone();
    if let CaptureRecord::Packet { bytes } = &mut changed.records[at] {
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
    }
    let r = workload::replay_client(&s, 0, &changed).expect("replay");
    assert_ne!(
        Some(r.digest),
        r.captured_digest,
        "one changed byte changes the output digest"
    );
    assert!(!r.matches());
    let (session, d) = r.divergence.expect("the comparison names the first difference");
    println!("one byte at record {at}: session {session}, {d:?}");
    assert!(
        d.captured.as_deref().is_some_and(|c| c.starts_with("Frame")),
        "{d:?}"
    );
}

/// Acceptance 3: a capture's memory is its ring: allocated once at its size, never grown, and the
/// records in it never pass it, over a run that writes far more than it holds.
#[test]
fn capture_memory_is_bounded_by_its_ring() {
    let ring = 64 << 10;
    let s = bad_wifi(2, 30, 0x5EED_0044);
    let taps: Vec<CaptureTap> = (0..2).map(|_| CaptureTap::new(ring).expect("tap")).collect();
    let heap: Vec<usize> = taps.iter().map(|t| t.lock().heap_bytes()).collect();
    workload::run_with_captures(&s, &taps).expect("run");
    for (i, tap) in taps.iter().enumerate() {
        let st = tap.stats();
        println!("client {i}: {st:?}");
        assert_eq!(tap.lock().heap_bytes(), heap[i], "the ring never grows");
        assert_eq!(heap[i], ring);
        assert!(st.peak_used_bytes <= ring && st.used_bytes <= ring, "{st:?}");
        assert!(st.evicted > 0, "the run wrote more than the ring holds: {st:?}");
        let written: usize = tap.lock().records().iter().map(CaptureRecord::ring_bytes).sum();
        assert_eq!(written, st.used_bytes);
    }
    // A ring that evicted the start of its only session holds no whole session: nothing to save.
    let dir = temp_dir("accept3");
    let r = taps[0].save(&dir.join("c.netcapture"), "x", false);
    assert_eq!(r.map(|_| ()).map_err(|e| e.code()), Err("capture_empty"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The harness hook: a failing run's rerun writes one capture per client with its replay command,
/// and each one replays.
#[test]
fn a_failing_run_writes_captures_that_replay() {
    let s = bad_wifi(2, 10, 0x5EED_0045);
    let dir = temp_dir("hook");
    let (paths, record) = write_failure_captures(&s, &dir).expect("captures written");
    assert_eq!(paths.len(), 2);
    for (i, path) in paths.iter().enumerate() {
        assert!(
            path.ends_with(format!("client-{i}.netcapture")),
            "{}",
            path.display()
        );
        let cmd = replay_command(path);
        assert!(
            cmd.starts_with(REPLAY_ENV) && cmd.contains("replay_capture_file"),
            "{cmd}"
        );
        let file = CaptureFile::load(path).expect("loads");
        let (scenario, client) = workload::capture_scenario(&file).expect("label");
        let r = workload::replay_client(&scenario, client, &file).expect("replay");
        assert!(r.matches(), "client {i}: {:?}", r.divergence);
        assert_eq!(r.record.corrections, record.clients[i].corrections);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A capture of another game is refused at the replay.
#[test]
fn a_capture_of_another_protocol_is_refused() {
    let s = bad_wifi(1, 4, 0x5EED_0046);
    let (_, files) = captured_run(&s, 1 << 20, "proto");
    let mut file = files[0].clone();
    if let Some(CaptureRecord::Session { protocol, .. }) = file.records.first_mut() {
        *protocol ^= 1;
    }
    let r = workload::replay_client(&s, 0, &file).expect("replay runs");
    // Every session is refused, so the replayed client never connects and the digests differ.
    assert!(!r.matches());
    assert_eq!(r.record.inputs_sent, 0);
}

/// The replay command a failing soak run prints: `REGIA_NET_SOAK_REPLAY=<file> cargo test -p
/// regia_net_soak --test capture_replay -- --ignored --exact replay_capture_file --nocapture`.
#[test]
#[ignore = "run by hand on a capture a failing soak run wrote (REGIA_NET_SOAK_REPLAY)"]
fn replay_capture_file() {
    let Some(path) = std::env::var_os(REPLAY_ENV) else {
        panic!("set {REPLAY_ENV} to a .netcapture a soak run wrote");
    };
    let file = CaptureFile::load(std::path::Path::new(&path)).expect("capture loads");
    let (scenario, client) = workload::capture_scenario(&file).expect("a net-soak capture");
    let r = workload::replay_client(&scenario, client, &file).expect("replay");
    println!(
        "{} client {client}: {} frames, {} sessions, corrections {} of {}, inputs {}, digest {:016x} (captured {:?})",
        scenario.label(),
        r.frames,
        r.sessions,
        r.record.corrections,
        r.record.corrections_checked,
        r.record.inputs_sent,
        r.digest,
        r.captured_digest.map(|d| format!("{d:016x}"))
    );
    if let Some((session, d)) = &r.divergence {
        println!(
            "first difference: session {session}, record {}: captured {:?}, replayed {:?}",
            d.index, d.captured, d.replayed
        );
    }
    assert!(r.matches(), "the capture replays exactly");
}
