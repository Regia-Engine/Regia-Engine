// SPDX-License-Identifier: MPL-2.0
//! Mute, block and report buffer (`docs/specs/59-voice-chat.md` § 2.13).

use super::packet::HEADER_LEN;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

/// Entries kept in each persisted account list.
pub const ACCOUNT_LIST_CAP: usize = 512;
/// Hard cap of the report ring in seconds.
pub const REPORT_BUFFER_MAX_SECONDS: u32 = 30;

/// One muted or blocked account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountEntry {
    /// Account id string.
    pub account: String,
    /// Optional display-name note.
    #[serde(default)]
    pub note: String,
}

/// An account list with a cap that evicts the oldest entry.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountList(pub Vec<AccountEntry>);

impl AccountList {
    /// The accounts, oldest first.
    pub fn accounts(&self) -> impl Iterator<Item = String> + '_ {
        self.0.iter().map(|e| e.account.clone())
    }

    /// Adds an account (or refreshes its note) and evicts the oldest entries above the cap.
    /// Returns whether it was newly added.
    pub fn add(&mut self, account: &str, note: &str) -> bool {
        if let Some(e) = self.0.iter_mut().find(|e| e.account == account) {
            e.note = note.to_string();
            return false;
        }
        self.0.push(AccountEntry {
            account: account.to_string(),
            note: note.to_string(),
        });
        if self.0.len() > ACCOUNT_LIST_CAP {
            let extra = self.0.len() - ACCOUNT_LIST_CAP;
            self.0.drain(..extra);
        }
        true
    }

    /// Removes an account; returns whether it was present.
    pub fn remove(&mut self, account: &str) -> bool {
        let before = self.0.len();
        self.0.retain(|e| e.account != account);
        self.0.len() != before
    }

    /// Whether the account is listed.
    #[must_use]
    pub fn contains(&self, account: &str) -> bool {
        self.0.iter().any(|e| e.account == account)
    }
}

/// A report handed to the game's sink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceReport {
    /// Reported sender slot.
    pub sender: u16,
    /// Channel the audio was heard on.
    pub channel: u16,
    /// Encoded packets of the last N seconds, oldest first.
    pub packets: Vec<Vec<u8>>,
}

/// Receives reports; the game routes them to its moderation backend.
pub trait VoiceReportSink {
    /// Takes ownership of a report.
    fn submit(&mut self, report: VoiceReport);
}

/// One recorded packet: its header timestamp (when the header parsed) and the wire bytes.
type RingEntry = (Option<u32>, Vec<u8>);

/// In-memory ring of the last N seconds of a speaker's already-encoded packets. Disabled
/// (`seconds = 0`) it keeps nothing. Never written to disk.
#[derive(Clone, Debug, Default)]
pub struct ReportRing {
    max_packets: usize,
    max_span: u32,
    rings: BTreeMap<u16, (u16, VecDeque<RingEntry>)>,
}

impl ReportRing {
    /// A ring holding `seconds` (capped at [`REPORT_BUFFER_MAX_SECONDS`]) of 50 packets/s.
    #[must_use]
    pub fn new(seconds: u32) -> Self {
        Self {
            max_packets: seconds.min(REPORT_BUFFER_MAX_SECONDS) as usize * 50,
            max_span: seconds.min(REPORT_BUFFER_MAX_SECONDS) * 48_000,
            rings: BTreeMap::new(),
        }
    }

    /// Records an encoded packet from `sender` on `channel`.
    pub fn record(&mut self, sender: u16, channel: u16, packet: &[u8]) {
        if self.max_packets == 0 {
            return;
        }
        let entry = self
            .rings
            .entry(sender)
            .or_insert_with(|| (channel, VecDeque::new()));
        entry.0 = channel;
        // The header timestamp (48 kHz units) bounds the ring by time, so paired-frame packets and
        // sparse comfort packets still cover at most N seconds.
        let ts = (packet.len() >= HEADER_LEN)
            .then(|| u32::from_le_bytes([packet[7], packet[8], packet[9], packet[10]]));
        entry.1.push_back((ts, packet.to_vec()));
        while entry.1.len() > self.max_packets {
            entry.1.pop_front();
        }
        if let Some(newest) = ts {
            while let Some(&(Some(oldest), _)) = entry.1.front() {
                // Wrapping distance, so a timestamp wrap does not evict everything.
                let span = newest.wrapping_sub(oldest);
                if span > self.max_span && span < 0x8000_0000 {
                    entry.1.pop_front();
                } else {
                    break;
                }
            }
        }
    }

    /// Builds a report for `sender`, if anything is buffered.
    #[must_use]
    pub fn report(&self, sender: u16) -> Option<VoiceReport> {
        let (channel, ring) = self.rings.get(&sender)?;
        Some(VoiceReport {
            sender,
            channel: *channel,
            packets: ring.iter().map(|(_, p)| p.clone()).collect(),
        })
    }

    /// Packets buffered for `sender`.
    #[must_use]
    pub fn len(&self, sender: u16) -> usize {
        self.rings.get(&sender).map_or(0, |r| r.1.len())
    }

    /// Whether nothing is buffered for anyone.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rings.values().all(|r| r.1.is_empty())
    }

    /// Clears everything (session end).
    pub fn clear(&mut self) {
        self.rings.clear();
    }
}
