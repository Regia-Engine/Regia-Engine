// SPDX-License-Identifier: MPL-2.0
//! The single-slot frame mailbox (spec 58 § 2.4) under overrun: newest wins, intermediate frames
//! are dropped and never replayed, repeats are not fresh, readers are independent. Deterministic
//! cadences, no sleeps.

use regia_xr::{XrFrameMailbox, XrFrameState};

fn frame(i: u64) -> XrFrameState {
    XrFrameState {
        frame_index: i,
        predicted_display_time_ns: 11_111_111 * (i as i64 + 1),
        period_ns: 11_111_111,
        should_render: !i.is_multiple_of(3),
    }
}

/// Catches an empty mailbox reporting a frame (a zero-initialised slot read as frame 0).
#[test]
fn an_empty_mailbox_has_nothing() {
    let mut mailbox = XrFrameMailbox::new();
    assert_eq!(mailbox.peek(), None);
    assert_eq!(mailbox.take_fresh(), None);
    mailbox.reset_reader();
    assert_eq!(mailbox.take_fresh(), None);
}

/// Catches a mailbox that queues: with the writer three frames ahead of the reader, the reader
/// sees every third frame and the other two are gone for good.
#[test]
fn a_writer_ahead_of_the_reader_drops_the_intermediate_frames() {
    let writer = XrFrameMailbox::new();
    let mut reader = writer.clone();
    let mut seen = Vec::new();
    let mut next = 0;
    for _ in 0..5 {
        for _ in 0..3 {
            writer.publish(frame(next));
            next += 1;
        }
        seen.push(reader.take_fresh().map(|f| f.frame_index));
        assert_eq!(reader.take_fresh(), None, "the slot was consumed");
    }
    assert_eq!(seen, [Some(2), Some(5), Some(8), Some(11), Some(14)]);
    let dropped = next - seen.len() as u64;
    assert_eq!(dropped, 10, "published minus read");
}

/// Catches a reader that never reports staleness: a reader polling faster than the writer
/// publishes sees "no new slot" until the next publish, and the count of such reads is exact.
#[test]
fn a_reader_ahead_of_the_writer_counts_stale_reads() {
    let writer = XrFrameMailbox::new();
    let mut reader = writer.clone();
    let mut stale = 0;
    let mut fresh = Vec::new();
    for step in 0..12_u64 {
        if step % 4 == 0 {
            writer.publish(frame(step / 4));
        }
        match reader.take_fresh() {
            Some(f) => fresh.push(f.frame_index),
            None => stale += 1,
        }
    }
    assert_eq!(fresh, [0, 1, 2]);
    assert_eq!(stale, 9);
}

/// Catches `peek` consuming: it never changes what `take_fresh` returns.
#[test]
fn peek_does_not_consume() {
    let writer = XrFrameMailbox::new();
    let mut reader = writer.clone();
    writer.publish(frame(4));
    for _ in 0..3 {
        assert_eq!(reader.peek(), Some(frame(4)));
    }
    assert_eq!(reader.take_fresh(), Some(frame(4)));
    assert_eq!(reader.peek(), Some(frame(4)), "still readable after it was taken");
    assert_eq!(reader.take_fresh(), None);
}

/// Catches fields getting mixed up in the sequence lock: extreme values and both `should_render`
/// settings survive the round trip.
#[test]
fn extreme_values_round_trip() {
    let writer = XrFrameMailbox::new();
    let reader = writer.clone();
    for state in [
        XrFrameState {
            frame_index: u64::MAX,
            predicted_display_time_ns: i64::MIN,
            period_ns: i64::MAX,
            should_render: true,
        },
        XrFrameState {
            frame_index: 0,
            predicted_display_time_ns: -1,
            period_ns: 0,
            should_render: false,
        },
    ] {
        writer.publish(state);
        assert_eq!(reader.peek(), Some(state));
    }
}

/// Catches shared reader state between clones: each clone tracks what it has seen on its own,
/// and a clone made after a read inherits that read.
#[test]
fn readers_are_independent_and_clones_inherit_their_source_position() {
    let writer = XrFrameMailbox::new();
    let mut a = writer.clone();
    let mut b = writer.clone();
    writer.publish(frame(1));
    assert_eq!(a.take_fresh(), Some(frame(1)));
    assert_eq!(b.take_fresh(), Some(frame(1)), "b has not seen it yet");
    assert_eq!(a.take_fresh(), None);
    let mut c = a.clone();
    assert_eq!(
        c.take_fresh(),
        None,
        "a clone of a reader that read frame 1 has read it too"
    );
    writer.publish(frame(2));
    assert_eq!(c.take_fresh(), Some(frame(2)));
    assert_eq!(a.take_fresh(), Some(frame(2)));
}

/// Catches reset forgetting the right thing: after a new session restarts at frame 0 a reader
/// that last saw frame 0 only sees it again once reset, while a restart from a higher index is
/// fresh without a reset.
#[test]
fn a_session_restart_needs_the_reset_only_when_the_index_repeats() {
    let writer = XrFrameMailbox::new();
    let mut reader = writer.clone();
    writer.publish(frame(0));
    assert_eq!(reader.take_fresh(), Some(frame(0)));
    // New session: frame 0 again, with a different time.
    let restarted = XrFrameState {
        predicted_display_time_ns: 5,
        ..frame(0)
    };
    writer.publish(restarted);
    assert_eq!(
        reader.take_fresh(),
        None,
        "same index as the last read looks stale"
    );
    reader.reset_reader();
    assert_eq!(reader.take_fresh(), Some(restarted));
    writer.publish(frame(7));
    assert_eq!(reader.take_fresh(), Some(frame(7)));
    writer.publish(frame(2));
    assert_eq!(
        reader.take_fresh(),
        Some(frame(2)),
        "an index going backwards is a new frame"
    );
}

/// Catches a reader seeing frames out of order or torn under a real writer thread: the freshest
/// slot only ever moves forward, each state is whole, and the last publish is what remains.
#[test]
fn take_fresh_is_monotonic_and_whole_under_a_concurrent_writer() {
    let writer = XrFrameMailbox::new();
    let mut reader = writer.clone();
    let handle = std::thread::spawn(move || {
        for i in 0..30_000 {
            writer.publish(frame(i));
        }
    });
    let mut last: Option<u64> = None;
    let mut reads = 0_u64;
    while !handle.is_finished() {
        if let Some(f) = reader.take_fresh() {
            assert_eq!(f, frame(f.frame_index), "torn slot");
            assert!(
                last.is_none_or(|l| f.frame_index > l),
                "{:?} after {last:?}",
                f.frame_index
            );
            last = Some(f.frame_index);
            reads += 1;
        }
    }
    handle.join().expect("writer");
    match reader.take_fresh() {
        Some(f) => assert_eq!(f, frame(29_999)),
        None => assert_eq!(last, Some(29_999)),
    }
    assert!(reads <= 30_000);
}
