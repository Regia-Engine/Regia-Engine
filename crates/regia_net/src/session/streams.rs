// SPDX-License-Identifier: MPL-2.0
//! Streams: one-off large reliable transfers paced under a share of the connection budget
//! (spec 77 § 8).
//!
//! A stream is up to 64 MiB, sent in chunks over the reliable channel. Each tick the sender gains
//! `stream_share` × the connection's per-tick byte budget in credit and sends at most that many
//! stream bytes, so gameplay traffic keeps the rest. At most 4 streams are in flight per
//! connection and direction. Streams are not replicated state: a drop fails every stream in flight
//! and nothing is resent after a reconnect.

use std::collections::{BTreeMap, VecDeque};

use crate::session::proto::{MAX_STREAM_CHUNK_BYTES, SessionMsg};

/// Largest stream, bytes.
pub const MAX_STREAM_BYTES: usize = 64 * 1024 * 1024;
/// Most streams in flight per connection and direction.
pub const MAX_STREAMS_IN_FLIGHT: usize = 4;

/// Bytes per second a connection's incoming streams must average over an idle window to count as
/// moving: a quarter of the lowest client rate would be 1 KiB/s, so a correct sender is far above it.
pub const MIN_STREAM_BYTES_PER_S: u64 = 256;
/// Failed stream ids remembered, so chunks already on their way are not a rule break.
const ABANDONED_STREAMS: usize = 16;

/// A peer broke the stream rules (too large, too many, unknown id, overrun).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamViolation;

/// A stream's id (per sender).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamId(pub u32);

/// Why a stream could not be started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamRefusal {
    /// Larger than 64 MiB.
    TooLarge,
    /// Already 4 streams in flight on this connection.
    TooMany,
}

/// Something that happened to a stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    /// More bytes arrived.
    Progress {
        /// Stream id.
        id: StreamId,
        /// Bytes received so far.
        received: u64,
        /// Total bytes.
        total: u64,
    },
    /// Every byte arrived.
    Complete {
        /// Stream id.
        id: StreamId,
        /// The sender's kind.
        kind: String,
        /// The data.
        bytes: Vec<u8>,
    },
    /// The connection dropped (or the sender aborted) before the stream completed.
    Failed {
        /// Stream id.
        id: StreamId,
        /// The reason code.
        reason: String,
    },
    /// A stream this side sent was fully handed to the transport.
    Sent {
        /// Stream id.
        id: StreamId,
    },
}

#[derive(Debug)]
struct OutStream {
    id: StreamId,
    kind: String,
    bytes: Vec<u8>,
    sent: usize,
    begun: bool,
}

#[derive(Debug)]
struct InStream {
    kind: String,
    total: u64,
    bytes: Vec<u8>,
}

/// The stream state of one connection: outgoing queue with pacing credit, incoming reassembly.
#[derive(Debug, Default)]
pub struct StreamLink {
    next_id: u32,
    out: VecDeque<OutStream>,
    credit: f64,
    sent_bytes: u64,
    incoming: BTreeMap<u32, InStream>,
    received_bytes: u64,
    /// `received_bytes` when the current idle window began, and when that was.
    progress: Option<(u64, u64)>,
    /// Streams this side gave up on; their late chunks are dropped.
    abandoned: VecDeque<u32>,
}

impl StreamLink {
    /// Queues a stream; it starts on the next pump.
    pub fn send(&mut self, kind: &str, bytes: Vec<u8>) -> Result<StreamId, StreamRefusal> {
        if bytes.len() > MAX_STREAM_BYTES {
            return Err(StreamRefusal::TooLarge);
        }
        if self.out.len() >= MAX_STREAMS_IN_FLIGHT {
            return Err(StreamRefusal::TooMany);
        }
        self.next_id = self.next_id.wrapping_add(1);
        let id = StreamId(self.next_id);
        self.out.push_back(OutStream {
            id,
            kind: kind.to_owned(),
            bytes,
            sent: 0,
            begun: false,
        });
        Ok(id)
    }

    /// Streams being sent.
    pub fn outgoing(&self) -> usize {
        self.out.len()
    }

    /// Stream payload bytes sent so far on this connection.
    pub fn sent_bytes(&self) -> u64 {
        self.sent_bytes
    }

    /// One tick of sending: adds `share × budget_bytes` of credit and returns the messages to send
    /// reliably (headers and chunks), plus `Sent` events for finished streams. Streams take turns
    /// chunk by chunk.
    pub fn pump(&mut self, budget_bytes: f64, share: f64, events: &mut Vec<StreamEvent>) -> Vec<SessionMsg> {
        let mut msgs = Vec::new();
        if self.out.is_empty() {
            self.credit = 0.0;
            return msgs;
        }
        let gain = (budget_bytes * share.clamp(0.0, 1.0)).max(0.0);
        // Keep at most one tick's gain (or one chunk) of credit, so an idle period never bursts.
        self.credit = (self.credit + gain).min(gain.max(MAX_STREAM_CHUNK_BYTES as f64));
        for s in &mut self.out {
            if !s.begun {
                s.begun = true;
                msgs.push(SessionMsg::StreamBegin {
                    id: s.id.0,
                    kind: s.kind.clone(),
                    total: s.bytes.len() as u64,
                });
            }
        }
        let mut progressed = true;
        while progressed && self.credit >= 1.0 {
            progressed = false;
            for s in &mut self.out {
                let left = s.bytes.len() - s.sent;
                if left == 0 {
                    continue;
                }
                let n = left.min(MAX_STREAM_CHUNK_BYTES).min(self.credit as usize);
                if n == 0 {
                    break;
                }
                msgs.push(SessionMsg::StreamChunk {
                    id: s.id.0,
                    bytes: s.bytes[s.sent..s.sent + n].to_vec(),
                });
                s.sent += n;
                self.credit -= n as f64;
                self.sent_bytes += n as u64;
                progressed = true;
            }
        }
        // Empty streams and finished ones.
        self.out.retain(|s| {
            let done = s.sent == s.bytes.len();
            if done {
                events.push(StreamEvent::Sent { id: s.id });
            }
            !done
        });
        msgs
    }

    /// Handles a received stream message. Returns `Err` when the peer broke the stream rules
    /// (too large, too many, unknown id, overrun), which is a malformed packet.
    pub fn receive(
        &mut self,
        msg: &SessionMsg,
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), StreamViolation> {
        match msg {
            SessionMsg::StreamBegin { id, kind, total } => {
                if *total > MAX_STREAM_BYTES as u64
                    || self.incoming.len() >= MAX_STREAMS_IN_FLIGHT
                    || self.incoming.contains_key(id)
                {
                    return Err(StreamViolation);
                }
                if *total == 0 {
                    events.push(StreamEvent::Complete {
                        id: StreamId(*id),
                        kind: kind.clone(),
                        bytes: Vec::new(),
                    });
                    return Ok(());
                }
                self.incoming.insert(
                    *id,
                    InStream {
                        kind: kind.clone(),
                        total: *total,
                        bytes: Vec::new(),
                    },
                );
                Ok(())
            }
            SessionMsg::StreamChunk { id, bytes } => {
                if self.abandoned.contains(id) {
                    return Ok(());
                }
                let s = self.incoming.get_mut(id).ok_or(StreamViolation)?;
                if (s.bytes.len() + bytes.len()) as u64 > s.total {
                    return Err(StreamViolation);
                }
                s.bytes.extend_from_slice(bytes);
                let received = s.bytes.len() as u64;
                self.received_bytes += bytes.len() as u64;
                events.push(StreamEvent::Progress {
                    id: StreamId(*id),
                    received,
                    total: s.total,
                });
                if received == s.total
                    && let Some(done) = self.incoming.remove(id)
                {
                    events.push(StreamEvent::Complete {
                        id: StreamId(*id),
                        kind: done.kind,
                        bytes: done.bytes,
                    });
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Bytes held for incoming streams.
    pub fn incoming_bytes(&self) -> usize {
        self.incoming.values().map(|s| s.bytes.len()).sum()
    }

    /// Fails every incoming stream when together they moved less than [`MIN_STREAM_BYTES_PER_S`] over
    /// `idle_ms`, releasing their buffers, so a peer cannot pin memory with streams it never finishes
    /// or trickles along (ADR-1892). Progress is counted per connection: the sender paces streams in
    /// turn, so one stream can wait a long time behind another while the connection is healthy.
    /// Call once per tick.
    pub fn expire_idle(&mut self, now_ms: u64, idle_ms: u64, events: &mut Vec<StreamEvent>) {
        if self.incoming.is_empty() {
            self.progress = None;
            return;
        }
        let Some((seen, since)) = self.progress else {
            self.progress = Some((self.received_bytes, now_ms));
            return;
        };
        if now_ms.saturating_sub(since) < idle_ms {
            return;
        }
        let floor = idle_ms.saturating_mul(MIN_STREAM_BYTES_PER_S) / 1000;
        if self.received_bytes.saturating_sub(seen) >= floor.max(1) {
            self.progress = Some((self.received_bytes, now_ms));
            return;
        }
        self.fail_incoming("stream_timeout", events);
    }

    /// Gives up on every incoming stream, with `reason`. Their later chunks are dropped.
    pub fn fail_incoming(&mut self, reason: &str, events: &mut Vec<StreamEvent>) {
        self.progress = None;
        for id in std::mem::take(&mut self.incoming).into_keys() {
            if self.abandoned.len() >= ABANDONED_STREAMS {
                self.abandoned.pop_front();
            }
            self.abandoned.push_back(id);
            events.push(StreamEvent::Failed {
                id: StreamId(id),
                reason: reason.to_owned(),
            });
        }
    }

    /// The connection dropped: every stream in flight (both directions) fails.
    pub fn fail_all(&mut self, reason: &str, events: &mut Vec<StreamEvent>) {
        for s in self.out.drain(..) {
            events.push(StreamEvent::Failed {
                id: s.id,
                reason: reason.to_owned(),
            });
        }
        for (id, _) in std::mem::take(&mut self.incoming) {
            events.push(StreamEvent::Failed {
                id: StreamId(id),
                reason: reason.to_owned(),
            });
        }
        self.credit = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn begin(id: u32) -> SessionMsg {
        SessionMsg::StreamBegin {
            id,
            kind: "k".into(),
            total: 100_000,
        }
    }

    fn chunk(id: u32, n: usize) -> SessionMsg {
        SessionMsg::StreamChunk {
            id,
            bytes: vec![0; n],
        }
    }

    /// Audit M7 (muse): streams that stop moving release their buffers. A stream waiting its turn
    /// behind a moving one is left alone: the sender paces them in order. Late chunks of a failed
    /// stream are dropped, so a correct sender is not disconnected for them.
    #[test]
    fn idle_incoming_streams_expire() {
        let mut l = StreamLink::default();
        let mut events = Vec::new();
        l.receive(&begin(1), &mut events).expect("ok");
        l.receive(&begin(2), &mut events).expect("ok");
        l.receive(&chunk(1, 30), &mut events).expect("ok");
        assert_eq!(l.incoming_bytes(), 30);
        events.clear();
        l.expire_idle(0, 100, &mut events);
        l.receive(&chunk(1, 30), &mut events).expect("ok");
        events.clear();
        // Stream 2 has had nothing yet, and stream 1 keeps the connection alive.
        l.expire_idle(99, 100, &mut events);
        l.expire_idle(100, 100, &mut events);
        assert!(events.is_empty(), "{events:?}");
        l.expire_idle(199, 100, &mut events);
        assert!(events.is_empty(), "{events:?}");
        l.expire_idle(200, 100, &mut events);
        assert_eq!(events.len(), 2, "both went: {events:?}");
        assert!(
            events
                .iter()
                .all(|e| matches!(e, StreamEvent::Failed { reason, .. } if reason == "stream_timeout"))
        );
        assert_eq!(l.incoming_bytes(), 0);
        events.clear();
        assert!(
            l.receive(&chunk(1, 30), &mut events).is_ok(),
            "a chunk already on its way"
        );
        assert!(events.is_empty());
        assert!(
            l.receive(&chunk(9, 1), &mut events).is_err(),
            "an id never begun still is a break"
        );
    }

    /// A sender that trickles just often enough to look alive does not hold its buffers.
    #[test]
    fn a_trickle_counts_as_idle() {
        let mut l = StreamLink::default();
        let mut events = Vec::new();
        l.receive(&begin(1), &mut events).expect("ok");
        l.expire_idle(0, 10_000, &mut events);
        for t in 1..=9u64 {
            l.receive(&chunk(1, 1), &mut events).expect("ok");
            l.expire_idle(t * 1000, 10_000, &mut events);
        }
        events.clear();
        l.expire_idle(10_000, 10_000, &mut events);
        assert!(
            matches!(events.as_slice(), [StreamEvent::Failed { .. }]),
            "{events:?}"
        );
    }

    /// A receive with nothing in flight never expires anything, however long it was quiet.
    #[test]
    fn a_new_stream_starts_a_fresh_idle_clock() {
        let mut l = StreamLink::default();
        let mut events = Vec::new();
        l.expire_idle(0, 100, &mut events);
        l.receive(
            &SessionMsg::StreamBegin {
                id: 1,
                kind: "k".into(),
                total: 10,
            },
            &mut events,
        )
        .expect("ok");
        l.expire_idle(5_000, 100, &mut events);
        l.expire_idle(5_099, 100, &mut events);
        assert!(events.is_empty());
    }

    #[test]
    fn paced_under_the_share_and_reassembled() {
        let mut tx = StreamLink::default();
        let mut rx = StreamLink::default();
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        tx.send("map", data.clone()).expect("start");
        let mut events = Vec::new();
        let mut ticks = 0;
        let mut done = None;
        while done.is_none() && ticks < 10_000 {
            ticks += 1;
            let msgs = tx.pump(4000.0, 0.25, &mut events);
            let payload: usize = msgs
                .iter()
                .map(|m| match m {
                    SessionMsg::StreamChunk { bytes, .. } => bytes.len(),
                    _ => 0,
                })
                .sum();
            assert!(payload <= 1000, "tick {ticks} sent {payload}");
            let mut rx_events = Vec::new();
            for m in &msgs {
                rx.receive(m, &mut rx_events).expect("valid");
            }
            for e in rx_events {
                if let StreamEvent::Complete { bytes, kind, .. } = e {
                    assert_eq!(kind, "map");
                    done = Some(bytes);
                }
            }
        }
        assert_eq!(done.as_deref(), Some(&data[..]));
        assert_eq!(ticks, 100);
    }

    #[test]
    fn limits() {
        let mut tx = StreamLink::default();
        assert_eq!(
            tx.send("x", vec![0; MAX_STREAM_BYTES + 1]),
            Err(StreamRefusal::TooLarge)
        );
        for _ in 0..4 {
            tx.send("x", vec![1; 10]).expect("fits");
        }
        assert_eq!(tx.send("x", vec![1; 10]), Err(StreamRefusal::TooMany));
        let mut rx = StreamLink::default();
        let mut ev = Vec::new();
        assert!(
            rx.receive(
                &SessionMsg::StreamBegin {
                    id: 1,
                    kind: "k".into(),
                    total: MAX_STREAM_BYTES as u64 + 1
                },
                &mut ev
            )
            .is_err()
        );
        assert!(
            rx.receive(
                &SessionMsg::StreamChunk {
                    id: 9,
                    bytes: vec![1]
                },
                &mut ev
            )
            .is_err()
        );
    }
}
