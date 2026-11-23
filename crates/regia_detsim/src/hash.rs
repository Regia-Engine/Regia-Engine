// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/desync_hash.rs lines 1-343 and
// ir-sim/src/state/desync_fold.rs lines 1-350, commit d6ed37677, tag regia-absorb-1), ADR-8012.
// Mark: adapt (ADR-8047). Named changes: (1) the system list is a parameter (`Systems<N>`, folded by a
// game's `SystemFold<N>`) instead of DO27's thirteen names and its game-state fold, which stays in the
// game; (2) the canonical float rule (`mix.rs`); (3) the process-wide environment hooks (sample period,
// injection plan, armed log tick) became values a host owns (`should_sample`'s period, `parse_inject`,
// `SystemLogArm`). The ring, the join, the report order and the log line shapes are DO27's.
// ADR-8547 (Regia's own code, no DO27 text): the checked fold (`fold_system_hashes`, `Folded`), the
// ring's opt-in raw-bits rows and partial flags (`SystemRing::with_raw`, `RingRow`) and the
// diagnosis over them (`diagnose`, `Diagnosis`, `diagnosis_lines`).

//! Per-system lockstep hashes: the desync diagnosis (spec 120 § 7.6, ADR-8047).
//!
//! A lockstep match keeps one wire checksum per tick (the game's world hash). When two peers'
//! checksums split, that word says *that* they split, not *where*. This module keeps the "where":
//!
//! - A game names its systems once ([`Systems`], at most [`MAX_SYSTEMS`]) and folds each system's
//!   state into its own [`Mix`] ([`SystemFold`]). [`system_hashes`] returns the vector, one word per
//!   system, with no allocation.
//! - Every peer keeps the last [`RING_TICKS`] vectors in a [`SystemRing`] (4 s at 60 Hz).
//! - On a split, the host asks the peers for their rows around the split tick
//!   ([`SystemRing::window`], at most [`MAX_TRACE_ROWS`]) and [`diverged_systems`] names every system
//!   that split and the first tick it did; [`divergence_lines`] writes the log lines.
//! - Tests and the desync gate nudge one system with [`inject_divergence`] and assert it is named at
//!   that tick.
//! - For diagnosis, a ring built with [`SystemRing::with_raw`] also keeps raw-bits words
//!   ([`FloatRule::Raw`]) and flags rows whose fold was partial (a failing `Serialize`);
//!   [`diagnose`] names a zero-sign or NaN-payload split at the tick it happened, which the canonical
//!   wire rule hides until it changes a value ([`Diagnosis::float_bits`]).
//!
//! [`Mix`] walks real state through a hashing `serde::Serializer`: no allocation, no text, and a field
//! added to a folded struct is covered without touching the fold when it serializes a value. Only
//! values are mixed, in order, with no field names, type names or field counts: a unit field, a
//! `#[serde(skip)]` field and a skipped `skip_serializing_if` field write nothing, and two structs
//! whose written values line up fold to the same word (Direct Order 27's encoding, kept as is,
//! ADR-8621). Fold only stable-order collections (`Vec`, arrays, `BTreeMap`), and entity rows in id
//! order.
//!
//! [`DesyncHash`] is spec 120's game-facing trait (§ 7.6); a game that implements [`SystemFold`]
//! implements it in a few lines (see the trait).
//!
//! ```
//! use regia_detsim::hash::{self, Mix, SystemFold, SystemRing, Systems};
//!
//! #[derive(Clone, Default)]
//! struct Sim { tick: u64, pos: Vec<[f32; 2]>, gold: [u32; 2] }
//!
//! impl SystemFold<2> for Sim {
//!     const SYSTEMS: Systems<2> = Systems::new(["units", "economy"]);
//!     fn fold_systems(&self, mix: &mut [Mix; 2]) {
//!         mix[0].fold(&self.pos);
//!         mix[1].fold(&self.gold);
//!     }
//!     fn inject(&mut self, system: usize) -> bool {
//!         match system {
//!             1 => self.gold[0] = self.gold[0].wrapping_add(1),
//!             _ => return false,
//!         }
//!         true
//!     }
//! }
//!
//! let mut a = Sim { pos: vec![[0.0, 0.0]], ..Sim::default() };
//! let mut b = a.clone();
//! let (mut ring_a, mut ring_b) = (SystemRing::<2>::new(), SystemRing::<2>::new());
//! for tick in 1..=10 {
//!     if tick == 7 {
//!         assert!(hash::inject_divergence(&mut b, "economy"));
//!     }
//!     a.tick = tick;
//!     b.tick = tick;
//!     ring_a.record(tick, &a);
//!     ring_b.record(tick, &b);
//! }
//! let report = hash::diverged_systems(&Sim::SYSTEMS, &ring_a.window(10, 10), &ring_b.window(10, 10));
//! assert_eq!(report.list(), "economy@7");
//! assert_eq!(report.last_agreed, Some(6));
//! ```

mod mix;

#[cfg(test)]
mod test_sim;
#[cfg(test)]
// The as-is DO27 test bodies stay as copied; the ignored perf probe `desync_hash_cost` times its loop.
#[expect(
    clippy::disallowed_methods,
    reason = "Instant::now in the #[ignore]d desync_hash_cost probe: prints a cost, never steps a sim (ADR-8367)"
)]
mod tests;

pub use mix::{
    CANONICAL_NAN_F32, CANONICAL_NAN_F64, FloatRule, Mix, NeverFails, canonical_f32_bits, canonical_f64_bits,
};

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// Tick value meaning "this peer was told the match split, but not the tick".
pub const SYSTEM_LOG_TICK_UNKNOWN: u64 = u64::MAX - 1;

/// Most systems one vector may hold (spec 120 § 7.6).
pub const MAX_SYSTEMS: usize = 64;

/// Ticks of per-tick vectors a peer keeps (4 s at 60 Hz).
pub const RING_TICKS: usize = 240;

/// Most rows a trace message may carry (the wire bound).
pub const MAX_TRACE_ROWS: usize = 96;

/// Tag of the [`Mix`] that combines a vector into one word ([`SystemHashes::combined`]). System
/// mixers use tags `1..=N`, so 0 never collides with one.
pub const COMBINED_TAG: u64 = 0;

/// A game's system names, in vector order. Report order and tie order follow this list.
///
/// Built in a `const` so a bad list fails the build: between 1 and [`MAX_SYSTEMS`] names, each
/// non-empty, ASCII `a-z`, `0-9` or `_` (they appear in log lines and inject plans), no duplicates.
///
/// ```
/// use regia_detsim::hash::Systems;
/// const SYSTEMS: Systems<3> = Systems::new(["units", "physics", "economy"]);
/// assert_eq!(SYSTEMS.index_of("physics"), Some(1));
/// ```
///
/// A duplicate (or an empty or badly spelled name) is a build error:
///
/// ```compile_fail
/// use regia_detsim::hash::Systems;
/// const SYSTEMS: Systems<2> = Systems::new(["units", "units"]);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Systems<const N: usize> {
    names: [&'static str; N],
}

const fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
            return false;
        }
        i += 1;
    }
    true
}

const fn same_name(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

impl<const N: usize> Systems<N> {
    /// The list. Panics (at compile time in a `const`) on an empty or too long list, a bad name or a
    /// duplicate.
    pub const fn new(names: [&'static str; N]) -> Self {
        assert!(N > 0, "a system list needs at least one system");
        assert!(
            N <= MAX_SYSTEMS,
            "a system list holds at most MAX_SYSTEMS (64) systems"
        );
        let mut i = 0;
        while i < N {
            assert!(
                valid_name(names[i]),
                "a system name is non-empty ASCII a-z, 0-9 or _"
            );
            let mut j = i + 1;
            while j < N {
                assert!(!same_name(names[i], names[j]), "duplicate system name");
                j += 1;
            }
            i += 1;
        }
        Self { names }
    }

    /// The names, in vector order.
    pub const fn names(&self) -> &[&'static str; N] {
        &self.names
    }

    /// Index of `name`, if it is on the list.
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| *n == name)
    }

    /// Name at `index`.
    pub fn name(&self, index: usize) -> Option<&'static str> {
        self.names.get(index).copied()
    }

    /// `name`'s word in `hashes`.
    pub fn get(&self, hashes: &SystemHashes<N>, name: &str) -> Option<u64> {
        self.index_of(name).map(|i| hashes.words[i])
    }

    /// Set `name`'s word in `hashes` (no-op for an unknown name).
    pub fn set(&self, hashes: &mut SystemHashes<N>, name: &str, word: u64) {
        if let Some(i) = self.index_of(name) {
            hashes.words[i] = word;
        }
    }

    /// First name whose word differs. `None` when the two match.
    pub fn first_diff(&self, left: &SystemHashes<N>, right: &SystemHashes<N>) -> Option<&'static str> {
        (0..N)
            .find(|i| left.words[*i] != right.words[*i])
            .map(|i| self.names[i])
    }

    /// `units {hex} physics {hex} ...` for a log line.
    pub fn fields(&self, hashes: &SystemHashes<N>) -> String {
        let mut out = String::new();
        for (name, word) in self.names.iter().zip(hashes.words) {
            if !out.is_empty() {
                out.push(' ');
            }
            let _ = write!(out, "{name} {word:016x}");
        }
        out
    }
}

/// One `u64` per system, in [`Systems`] order.
///
/// Serializes as a struct with one field `words` holding an `N`-tuple of `u64` (the shape a derive
/// gives a `[u64; N]` field, so a 13-system vector is wire-compatible with DO27's).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SystemHashes<const N: usize> {
    /// The words, in [`Systems`] order.
    pub words: [u64; N],
}

impl<const N: usize> Default for SystemHashes<N> {
    fn default() -> Self {
        Self { words: [0; N] }
    }
}

impl<const N: usize> SystemHashes<N> {
    /// All `N` words folded into one (a [`Mix`] tagged [`COMBINED_TAG`]): a world hash for a game
    /// with no checksum of its own.
    pub fn combined(&self) -> u64 {
        let mut mix = Mix::new(COMBINED_TAG);
        for word in self.words {
            mix.u64(word);
        }
        mix.finish()
    }
}

struct Words<'a, const N: usize>(&'a [u64; N]);

impl<const N: usize> Serialize for Words<'_, N> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeTuple;
        let mut tuple = serializer.serialize_tuple(N)?;
        for word in self.0 {
            tuple.serialize_element(word)?;
        }
        tuple.end()
    }
}

impl<const N: usize> Serialize for SystemHashes<N> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = serializer.serialize_struct("SystemHashes", 1)?;
        st.serialize_field("words", &Words(&self.words))?;
        st.end()
    }
}

struct OwnedWords<const N: usize>([u64; N]);

impl<'de, const N: usize> Deserialize<'de> for OwnedWords<N> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visit<const N: usize>;
        impl<'de, const N: usize> serde::de::Visitor<'de> for Visit<N> {
            type Value = OwnedWords<N>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{N} u64 words")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut words = [0u64; N];
                for (i, word) in words.iter_mut().enumerate() {
                    *word = seq
                        .next_element()?
                        .ok_or_else(|| serde::de::Error::invalid_length(i, &self))?;
                }
                Ok(OwnedWords(words))
            }
        }
        deserializer.deserialize_tuple(N, Visit::<N>)
    }
}

impl<'de, const N: usize> Deserialize<'de> for SystemHashes<N> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename = "SystemHashes")]
        struct Raw<const N: usize> {
            words: OwnedWords<N>,
        }
        let raw = Raw::<N>::deserialize(deserializer)?;
        Ok(Self { words: raw.words.0 })
    }
}

/// A game's per-system fold: the state behind each name in [`SystemFold::SYSTEMS`].
///
/// Fold each system's values into `mix[index]` (the mixers arrive tagged `index + 1`). One pass over
/// id-ordered entity rows may feed several systems (a body's position to one, its velocity to
/// another). Do not allocate: the fold runs after every lockstep step.
pub trait SystemFold<const N: usize> {
    /// The system names, in vector order.
    const SYSTEMS: Systems<N>;

    /// Fold every system's state into its mixer.
    fn fold_systems(&self, mix: &mut [Mix; N]);

    /// Nudge system `system` (an index into [`SystemFold::SYSTEMS`]) so its fold changes and, where
    /// the step does not read the value back into another system, nothing else does. Play tests and
    /// the desync gate only. Returns `false` when the game cannot nudge that system (the default).
    fn inject(&mut self, system: usize) -> bool {
        let _ = system;
        false
    }
}

/// Fold every system of `state`. Does not read or write the game's wire checksum. No allocation of
/// its own.
///
/// Debug builds assert that no `Serialize` failed during the fold (a failure leaves a word that
/// covers part of the state); [`fold_system_hashes`] returns the flag instead of asserting, and
/// [`SystemRing::record`] keeps it on the row.
pub fn system_hashes<S: SystemFold<N> + ?Sized, const N: usize>(state: &S) -> SystemHashes<N> {
    let folded = fold_system_hashes(state, FloatRule::Canonical);
    debug_assert!(
        !folded.partial,
        "a Serialize failed while folding the desync hash: the words cover part of the state"
    );
    folded.hashes
}

/// A folded vector and whether any of its words is partial.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Folded<const N: usize> {
    /// The words.
    pub hashes: SystemHashes<N>,
    /// A `Serialize` failed in at least one system's fold ([`Mix::errors`]): that word covers part
    /// of the state.
    pub partial: bool,
}

/// Finish `N` system mixers into a vector, with the partial flag ([`Mix::errors`]). The mixers must
/// be tagged `index + 1` (as [`fold_system_hashes`] makes them).
pub fn finish_mixers<const N: usize>(mix: [Mix; N]) -> Folded<N> {
    let partial = mix.iter().any(|m| m.errors() > 0);
    Folded {
        hashes: SystemHashes {
            words: mix.map(Mix::finish),
        },
        partial,
    }
}

/// Fold every system of `state` under float rule `rule`, without asserting: the result says
/// whether a `Serialize` failed. [`FloatRule::Canonical`] gives [`system_hashes`]'s words.
pub fn fold_system_hashes<S: SystemFold<N> + ?Sized, const N: usize>(
    state: &S,
    rule: FloatRule,
) -> Folded<N> {
    let mut mix: [Mix; N] = std::array::from_fn(|i| Mix::with_rule(i as u64 + 1, rule));
    state.fold_systems(&mut mix);
    finish_mixers(mix)
}

/// Nudge one system's state by name (play tests and the desync gate only). `false` for an unknown
/// name or a system the game cannot nudge.
pub fn inject_divergence<S: SystemFold<N> + ?Sized, const N: usize>(state: &mut S, system: &str) -> bool {
    match S::SYSTEMS.index_of(system) {
        Some(index) => state.inject(index),
        None => false,
    }
}

/// Spec 120's game-facing desync hashing (§ 7.6).
///
/// A game with a [`SystemFold`] implements it like this (its own checksum, if it has one, is the
/// world hash; otherwise [`SystemHashes::combined`]):
///
/// ```
/// use regia_detsim::hash::{self, DesyncHash, Mix, SystemFold, Systems};
///
/// struct Sim { gold: u32 }
/// impl SystemFold<1> for Sim {
///     const SYSTEMS: Systems<1> = Systems::new(["economy"]);
///     fn fold_systems(&self, mix: &mut [Mix; 1]) { mix[0].u64(u64::from(self.gold)); }
/// }
/// impl DesyncHash for Sim {
///     fn world_hash(&self, _tick: u64) -> u64 { hash::system_hashes(self).combined() }
///     fn subsystem_hashes(&self, _tick: u64, out: &mut Vec<(&'static str, u64)>) {
///         hash::write_rows(&Self::SYSTEMS, &hash::system_hashes(self), out);
///     }
/// }
///
/// let mut rows = Vec::with_capacity(1);
/// Sim { gold: 3 }.subsystem_hashes(9, &mut rows);
/// assert_eq!(rows[0].0, "economy");
/// ```
pub trait DesyncHash {
    /// One 64-bit hash of the whole state after stepping `tick` (cheap; sent every `hash_every`
    /// ticks).
    fn world_hash(&self, tick: u64) -> u64;
    /// Named per-subsystem hashes for the probe (at most [`MAX_SYSTEMS`]), folded after every step
    /// into a ring. Clear `out` first.
    fn subsystem_hashes(&self, tick: u64, out: &mut Vec<(&'static str, u64)>);
}

/// Replace `out` with `(name, word)` pairs in [`Systems`] order. Allocates only when `out` has less
/// than `N` capacity.
pub fn write_rows<const N: usize>(
    systems: &Systems<N>,
    hashes: &SystemHashes<N>,
    out: &mut Vec<(&'static str, u64)>,
) {
    out.clear();
    out.extend(systems.names.iter().copied().zip(hashes.words));
}

/// Report that names the first system whose hash differs.
pub fn desync_report<const N: usize>(
    systems: &Systems<N>,
    tick: u64,
    left_label: &str,
    left: &SystemHashes<N>,
    right_label: &str,
    right: &SystemHashes<N>,
) -> String {
    let first = systems.first_diff(left, right).unwrap_or("none");
    let mut out = format!("desync tick {tick} first {first}\n");
    for (i, name) in systems.names.iter().enumerate() {
        let _ = writeln!(
            out,
            "  {name} {left_label}={:016x} {right_label}={:016x}",
            left.words[i], right.words[i]
        );
    }
    out
}

/// One tick of the vector, as it rides the wire and the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceRow<const N: usize> {
    /// The lockstep step label (the same on every peer).
    pub tick: u64,
    /// The vector after that step.
    pub hashes: SystemHashes<N>,
}

/// The last [`RING_TICKS`] per-tick vectors of one peer. Allocates once, in [`SystemRing::new`] (or
/// [`SystemRing::with_raw`]).
///
/// Besides the canonical rows, the ring keeps one partial flag per row ([`Folded::partial`]) and,
/// when built with [`SystemRing::with_raw`], one raw-bits vector per row ([`FloatRule::Raw`]):
/// [`SystemRing::window_rows`] returns them for [`diagnose`]. The raw rows are diagnosis only; the
/// canonical rows ([`SystemRing::window`]) are what peers exchange.
#[derive(Clone, Debug)]
pub struct SystemRing<const N: usize> {
    rows: VecDeque<TraceRow<N>>,
    partial: VecDeque<bool>,
    raw: Option<VecDeque<Option<SystemHashes<N>>>>,
    partial_rows: u64,
}

/// One ring row with its diagnosis extras ([`SystemRing::window_rows`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingRow<const N: usize> {
    /// The lockstep step label.
    pub tick: u64,
    /// The canonical vector (the one peers exchange).
    pub hashes: SystemHashes<N>,
    /// The raw-bits vector ([`FloatRule::Raw`]), on a ring built with [`SystemRing::with_raw`].
    pub raw: Option<SystemHashes<N>>,
    /// A `Serialize` failed while folding this row: some words cover part of the state.
    pub partial: bool,
}

impl<const N: usize> RingRow<N> {
    /// A whole, canonical-only row.
    pub fn new(tick: u64, hashes: SystemHashes<N>) -> Self {
        Self {
            tick,
            hashes,
            raw: None,
            partial: false,
        }
    }

    /// The canonical row.
    pub fn trace(&self) -> TraceRow<N> {
        TraceRow {
            tick: self.tick,
            hashes: self.hashes,
        }
    }
}

impl<const N: usize> Default for SystemRing<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> SystemRing<N> {
    /// An empty ring with room for [`RING_TICKS`] rows.
    pub fn new() -> Self {
        Self {
            rows: VecDeque::with_capacity(RING_TICKS),
            partial: VecDeque::with_capacity(RING_TICKS),
            raw: None,
            partial_rows: 0,
        }
    }

    /// An empty ring that also keeps a raw-bits vector per row (the diagnose ring, ADR-8547).
    /// [`SystemRing::record`] then folds the state twice (once per float rule) and the ring holds
    /// twice the words. Off by default; the canonical rows are the same as [`SystemRing::new`]'s.
    pub fn with_raw() -> Self {
        Self {
            raw: Some(VecDeque::with_capacity(RING_TICKS)),
            ..Self::new()
        }
    }

    /// Whether this ring keeps raw-bits rows ([`SystemRing::with_raw`]).
    pub fn raw_enabled(&self) -> bool {
        self.raw.is_some()
    }

    /// Rows recorded with a partial fold since the ring was made (they may have scrolled out).
    pub fn partial_rows(&self) -> u64 {
        self.partial_rows
    }

    /// Hash `state` and keep the row under `tick` (the lockstep step label, the same on every peer).
    /// A raw ring also folds the raw-bits vector. A failing `Serialize` marks the row partial
    /// ([`RingRow::partial`]) and logs a warning on the first such row.
    pub fn record<S: SystemFold<N> + ?Sized>(&mut self, tick: u64, state: &S) {
        let folded = fold_system_hashes(state, FloatRule::Canonical);
        let raw = self
            .raw
            .is_some()
            .then(|| fold_system_hashes(state, FloatRule::Raw).hashes);
        self.push_row(RingRow {
            tick,
            hashes: folded.hashes,
            raw,
            partial: folded.partial,
        });
    }

    /// Keep `hashes` under `tick`. A label at or below the newest row (a rewind) replaces the rows
    /// from there on; past [`RING_TICKS`] rows the oldest drops.
    pub fn push(&mut self, tick: u64, hashes: SystemHashes<N>) {
        self.push_row(RingRow::new(tick, hashes));
    }

    /// Keep `row` (with its raw vector, on a raw ring, and its partial flag). Same rewind and drop
    /// rules as [`SystemRing::push`]. A raw vector given to a ring without raw rows is dropped.
    pub fn push_row(&mut self, row: RingRow<N>) {
        // A re-run label (a rewind) replaces the rows from there on.
        while self.rows.back().is_some_and(|kept| kept.tick >= row.tick) {
            self.rows.pop_back();
            self.partial.pop_back();
            if let Some(raw) = self.raw.as_mut() {
                raw.pop_back();
            }
        }
        if self.rows.len() == RING_TICKS {
            self.rows.pop_front();
            self.partial.pop_front();
            if let Some(raw) = self.raw.as_mut() {
                raw.pop_front();
            }
        }
        if row.partial {
            if self.partial_rows == 0 {
                tracing::warn!(
                    target: "regia_detsim::hash",
                    tick = row.tick,
                    "desync hash: a Serialize failed while folding; this row's words cover part of the state"
                );
            }
            self.partial_rows += 1;
        }
        self.rows.push_back(row.trace());
        self.partial.push_back(row.partial);
        if let Some(raw) = self.raw.as_mut() {
            raw.push_back(row.raw);
        }
    }

    /// Drop every row (a restore that starts a new timeline, ADR-8621). A raw ring stays raw; the
    /// [`SystemRing::partial_rows`] count is kept (it counts since the ring was made).
    pub fn clear(&mut self) {
        self.rows.clear();
        self.partial.clear();
        if let Some(raw) = self.raw.as_mut() {
            raw.clear();
        }
    }

    /// Rows held.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// True when no row is held.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The rows with `to - count < tick <= to`, oldest first, at most [`MAX_TRACE_ROWS`].
    pub fn window(&self, to: u64, count: usize) -> Vec<TraceRow<N>> {
        let count = count.min(MAX_TRACE_ROWS);
        let from = to.saturating_add(1).saturating_sub(count as u64);
        self.rows
            .iter()
            .filter(|row| row.tick >= from && row.tick <= to)
            .copied()
            .collect()
    }

    /// [`SystemRing::window`] with each row's raw vector and partial flag ([`RingRow`]).
    pub fn window_rows(&self, to: u64, count: usize) -> Vec<RingRow<N>> {
        let count = count.min(MAX_TRACE_ROWS);
        let from = to.saturating_add(1).saturating_sub(count as u64);
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.tick >= from && row.tick <= to)
            .map(|(i, row)| RingRow {
                tick: row.tick,
                hashes: row.hashes,
                raw: self.raw.as_ref().and_then(|raw| raw.get(i).copied().flatten()),
                partial: self.partial.get(i).copied().unwrap_or(false),
            })
            .collect()
    }
}

/// Saves the canonical rows (oldest first): the system count `N`, whether the ring keeps raw rows
/// ([`SystemRing::with_raw`]), then each row's tick and words (bundles, late joiners). `load` refuses
/// another `N`, more than [`RING_TICKS`] rows, and ticks that do not increase (`bad_value`).
///
/// Raw words and partial flags are diagnosis state, not sim state (ADR-8621): they are not saved. A
/// loaded row is whole and has no raw words (a raw ring keeps an empty raw slot per loaded row), so
/// the flags of rows recorded after the load stay on their own ticks.
impl<const N: usize> crate::state::DetState for SystemRing<N> {
    const STATE_TAG: [u8; 8] = *b"sysring\0";
    const STATE_VERSION: u32 = 1;

    fn save(&self, w: &mut crate::state::DetWriter) {
        w.length(N);
        w.bool(self.raw.is_some());
        w.length(self.rows.len());
        for row in &self.rows {
            w.u64(row.tick);
            for word in row.hashes.words {
                w.u64(word);
            }
        }
    }

    fn load(r: &mut crate::state::DetReader<'_>) -> Result<Self, crate::state::DetStateError> {
        use crate::state::DetStateError;
        let n = r.length()?;
        if n != N {
            return Err(DetStateError::bad_value(format_args!(
                "a ring of {n} systems, this build has {N}"
            )));
        }
        let raw = r.bool()?;
        let rows = r.length()?;
        if rows > RING_TICKS {
            return Err(DetStateError::bad_value(format_args!(
                "{rows} ring rows, at most {RING_TICKS}"
            )));
        }
        let mut ring = if raw {
            SystemRing::with_raw()
        } else {
            SystemRing::new()
        };
        for _ in 0..rows {
            let tick = r.u64()?;
            if ring.rows.back().is_some_and(|row| row.tick >= tick) {
                return Err(DetStateError::bad_value("ring ticks do not increase"));
            }
            let mut hashes = SystemHashes::<N>::default();
            for word in &mut hashes.words {
                *word = r.u64()?;
            }
            // Through `push_row`, so the partial flags and raw slots stay one per row.
            ring.push_row(RingRow::new(tick, hashes));
        }
        Ok(ring)
    }
}

/// One system that split, and the first tick it did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Divergence {
    /// The system's name.
    pub system: &'static str,
    /// The first shared tick on which its words differed.
    pub first_tick: u64,
}

/// Which side of a join a row came from: `left` is the first slice given, `right` the second.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RowSide {
    /// The first slice ([`diverged_systems`]'s `left`, the agent's own rows).
    Left,
    /// The second slice ([`diverged_systems`]'s `right`, the other run's rows).
    Right,
}

impl RowSide {
    /// `left` or `right` (the agent protocol's `report.unmatched[].side`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
        }
    }
}

/// A row the join could not pair with a row of the other side (ADR-8724).
///
/// Rows at one tick pair up in input order (the first left row with the first right row, and so
/// on). A row is unmatched when the other side has no row at its tick (`tick_shared` false: it was
/// not compared), or when its side holds more rows at that tick than the other (`tick_shared` true:
/// a surplus row; it was still compared with every row the other side has at that tick).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnmatchedRow {
    /// The side the row came from.
    pub side: RowSide,
    /// The row's tick.
    pub tick: u64,
    /// The row's position in its side's slice.
    pub index: usize,
    /// The other side has at least one row at this tick (the row is a surplus row and was compared).
    pub tick_shared: bool,
}

/// What two peers' traces say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DivergenceReport {
    /// Ticks both peers had a row for.
    pub compared: usize,
    /// Latest tick before the earliest split on which every system agreed.
    pub last_agreed: Option<u64>,
    /// Split systems, earliest tick first, then in [`Systems`] order.
    pub systems: Vec<Divergence>,
    /// Rows without a partner on the other side, by tick, then left before right, then position
    /// (ADR-8724).
    pub unmatched: Vec<UnmatchedRow>,
}

impl DivergenceReport {
    /// The earliest split system (ties go to the earlier name in [`Systems`] order).
    pub fn root(&self) -> Option<Divergence> {
        self.systems.first().copied()
    }

    /// `units@4231,economy@4231,production@4233`
    pub fn list(&self) -> String {
        let parts: Vec<String> = self
            .systems
            .iter()
            .map(|d| format!("{}@{}", d.system, d.first_tick))
            .collect();
        parts.join(",")
    }

    /// The ticks at which one side held more rows than the other ([`UnmatchedRow::tick_shared`]),
    /// each once, ascending.
    pub fn surplus_ticks(&self) -> Vec<u64> {
        let mut ticks: Vec<u64> = self
            .unmatched
            .iter()
            .filter(|row| row.tick_shared)
            .map(|row| row.tick)
            .collect();
        ticks.dedup();
        ticks
    }
}

/// One side's rows at one tick: `(position in the input slice, words)`, in input order.
type TickRows<'a, const N: usize> = Vec<(usize, &'a SystemHashes<N>)>;

/// `rows` with each row's position.
fn indexed<T>(rows: &[T]) -> Vec<(usize, &T)> {
    rows.iter().enumerate().collect()
}

/// Borrowed `(position, row)` pairs of rows that already carry their position.
fn kept<T>(rows: &[(usize, T)]) -> Vec<(usize, &T)> {
    rows.iter().map(|(index, row)| (*index, row)).collect()
}

/// The join behind [`diverged_systems`]: the report and the ticks it compared, ascending. `left`
/// and `right` are `(position in the caller's slice, row)`.
fn join_rows<const N: usize>(
    systems: &Systems<N>,
    left: &[(usize, &TraceRow<N>)],
    right: &[(usize, &TraceRow<N>)],
) -> (DivergenceReport, Vec<u64>) {
    // Every row at a tick takes part (ADR-8724): group both sides by tick, keeping input order.
    let mut by_tick: BTreeMap<u64, (TickRows<'_, N>, TickRows<'_, N>)> = BTreeMap::new();
    for &(index, row) in left {
        by_tick.entry(row.tick).or_default().0.push((index, &row.hashes));
    }
    for &(index, row) in right {
        by_tick.entry(row.tick).or_default().1.push((index, &row.hashes));
    }
    let mut first: [Option<u64>; N] = [None; N];
    let mut agreed_through: Option<u64> = None;
    let mut earliest_split: Option<u64> = None;
    let mut ticks: Vec<u64> = Vec::new();
    let mut unmatched: Vec<UnmatchedRow> = Vec::new();
    for (tick, (a, b)) in &by_tick {
        let tick = *tick;
        let tick_shared = !a.is_empty() && !b.is_empty();
        let paired = a.len().min(b.len());
        for (side, rows) in [(RowSide::Left, a), (RowSide::Right, b)] {
            for &(index, _) in rows.iter().skip(paired) {
                unmatched.push(UnmatchedRow {
                    side,
                    tick,
                    index,
                    tick_shared,
                });
            }
        }
        let (Some(&(_, reference)), true) = (a.first(), tick_shared) else {
            continue;
        };
        ticks.push(tick);
        // Every left row against every right row: a system agrees at this tick only when all of
        // its words there are one value (two different rows on one side are not agreement).
        let mut split = false;
        for (s, first) in first.iter_mut().enumerate() {
            let word = reference.words[s];
            if a.iter().chain(b.iter()).any(|(_, row)| row.words[s] != word) {
                split = true;
                first.get_or_insert(tick);
            }
        }
        if split {
            earliest_split.get_or_insert(tick);
        } else if earliest_split.is_none() {
            agreed_through = Some(tick);
        }
    }
    let mut found: Vec<Divergence> = first
        .iter()
        .enumerate()
        .filter_map(|(i, first)| {
            first.map(|first_tick| Divergence {
                system: systems.names[i],
                first_tick,
            })
        })
        .collect();
    found.sort_by_key(|d| d.first_tick);
    let report = DivergenceReport {
        compared: ticks.len(),
        last_agreed: agreed_through,
        systems: found,
        unmatched,
    };
    (report, ticks)
}

/// Join two peers' rows on tick and find each system's first differing tick.
///
/// Every row at a tick takes part (ADR-8724): when a side holds several rows at one tick, each left
/// row is compared with each right row there, so a system splits at that tick unless all of its
/// words there are one value. Rows pair in input order; a row with no partner (its tick is missing
/// on the other side, or its side holds more rows at that tick) is listed in
/// [`DivergenceReport::unmatched`]. Ticks only one side has are not compared. The result does not
/// depend on the order of the rows within a slice, apart from which rows are named unmatched.
pub fn diverged_systems<const N: usize>(
    systems: &Systems<N>,
    left: &[TraceRow<N>],
    right: &[TraceRow<N>],
) -> DivergenceReport {
    join_rows(systems, &indexed(left), &indexed(right)).0
}

/// The log lines that name the split: one human line and one `key value` line a tool can read.
pub fn divergence_lines(
    split_tick: u64,
    left_label: &str,
    right_label: &str,
    report: &DivergenceReport,
) -> Vec<String> {
    let Some(root) = report.root() else {
        return vec![format!(
            "lockstep desync systems tick {split_tick} {left_label} vs {right_label}: no system differs in the {} shared ticks",
            report.compared
        )];
    };
    vec![
        format!(
            "lockstep desync systems tick {split_tick} {left_label} vs {right_label}: first {} at tick {} ({})",
            root.system,
            root.first_tick,
            report.list().replace(',', " ")
        ),
        format!(
            "lockstep desync verdict split {split_tick} left {left_label} right {right_label} root {} first {} compared {} last_agreed {} systems {}",
            root.system,
            root.first_tick,
            report.compared,
            report.last_agreed.map_or("none".to_string(), |t| t.to_string()),
            report.list()
        ),
    ]
}

/// A system whose raw-bits words ([`FloatRule::Raw`]) split before its canonical words did, or
/// while they never did: the split began as a zero sign or a NaN payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FloatBitsSplit {
    /// The system's name.
    pub system: &'static str,
    /// The first shared tick on which its raw words differed.
    pub raw_tick: u64,
    /// The first shared tick on which its canonical words differed (`None`: not in the compared
    /// rows).
    pub canonical_tick: Option<u64>,
}

/// The one-word answer of a [`Diagnosis`]: whether two peers' rows agree, and if not, which view
/// says so first (ADR-8621). Only [`Verdict::Same`] means agreement. Non-exhaustive: a later view
/// adds a variant without a break.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Verdict {
    /// At least one tick was compared, every compared row was whole, and the canonical and raw words
    /// agree on every compared tick.
    Same,
    /// The canonical words (the words peers exchange) split: [`Diagnosis::canonical`] names them.
    Canonical,
    /// The canonical words agree, the raw-bits words split (a zero sign or a NaN payload):
    /// [`Diagnosis::float_bits`] names them.
    FloatBits,
    /// The words agree, but a compared row was partial (a `Serialize` failed, so the state after
    /// the error was not hashed): agreement is not shown from [`Diagnosis::partial_from`] on.
    Partial,
    /// The two sides share no tick: nothing was compared.
    NoOverlap,
}

impl Verdict {
    /// `same`, `canonical`, `float_bits`, `partial` or `no_overlap` (the agent protocol's
    /// `report.verdict`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Same => "same",
            Self::Canonical => "canonical",
            Self::FloatBits => "float_bits",
            Self::Partial => "partial",
            Self::NoOverlap => "no_overlap",
        }
    }
}

/// What [`diagnose`] finds in two peers' [`RingRow`]s.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnosis {
    /// The canonical report ([`diverged_systems`] over the canonical words).
    pub canonical: DivergenceReport,
    /// The same report over the raw-bits words, when both sides carried raw rows.
    pub raw: Option<DivergenceReport>,
    /// Systems whose raw words split first (zero sign or NaN payload), earliest raw tick first.
    pub float_bits: Vec<FloatBitsSplit>,
    /// The earliest compared tick on which either side's row was partial (a failing `Serialize`).
    pub partial_from: Option<u64>,
}

impl Diagnosis {
    /// The answer in one word: a canonical split first, then a raw-bits split, then a partial row,
    /// then no shared tick; [`Verdict::Same`] only when none of them holds.
    pub fn verdict(&self) -> Verdict {
        if !self.canonical.systems.is_empty() {
            Verdict::Canonical
        } else if !self.float_bits.is_empty() {
            Verdict::FloatBits
        } else if self.partial_from.is_some() {
            Verdict::Partial
        } else if self.canonical.compared == 0 {
            Verdict::NoOverlap
        } else {
            Verdict::Same
        }
    }

    /// True only when the rows agree ([`Verdict::Same`]): some tick was compared, no compared row
    /// was partial, and neither the canonical nor the raw words split.
    pub fn same(&self) -> bool {
        self.verdict() == Verdict::Same
    }
}

/// Pull `last_agreed` back below the first partial tick: a partial row's equal words do not show
/// that the state agreed (the part after the failing `Serialize` was not hashed).
fn agreed_before(last_agreed: Option<u64>, partial_from: Option<u64>, compared: &[u64]) -> Option<u64> {
    match (last_agreed, partial_from) {
        (Some(agreed), Some(partial)) if agreed >= partial => {
            compared.iter().copied().filter(|&tick| tick < partial).max()
        }
        _ => last_agreed,
    }
}

/// Join two peers' ring rows on tick: the canonical report, the raw-bits report (ticks where both
/// rows carry raw words) and the systems whose split shows in the raw words first.
///
/// ```
/// use regia_detsim::hash::{self, Mix, SystemFold, SystemRing, Systems};
///
/// #[derive(Clone)]
/// struct Sim { zero: f32, inv: f32 }
/// impl SystemFold<1> for Sim {
///     const SYSTEMS: Systems<1> = Systems::new(["motion"]);
///     fn fold_systems(&self, mix: &mut [Mix; 1]) {
///         mix[0].f32(self.zero);
///         mix[0].f32(self.inv);
///     }
/// }
/// let (mut a, mut b) = (Sim { zero: 0.0, inv: 0.0 }, Sim { zero: 0.0, inv: 0.0 });
/// let (mut ring_a, mut ring_b) = (SystemRing::<1>::with_raw(), SystemRing::<1>::with_raw());
/// for tick in 1..=6 {
///     if tick == 3 {
///         b.zero = -0.0; // the sign of a zero splits here
///     }
///     if tick == 5 {
///         (a.inv, b.inv) = (1.0 / a.zero, 1.0 / b.zero); // and becomes +inf / -inf here
///     }
///     ring_a.record(tick, &a);
///     ring_b.record(tick, &b);
/// }
/// let d = hash::diagnose(&Sim::SYSTEMS, &ring_a.window_rows(6, 6), &ring_b.window_rows(6, 6));
/// assert_eq!(d.canonical.list(), "motion@5");
/// assert_eq!(d.float_bits[0].raw_tick, 3);
/// ```
pub fn diagnose<const N: usize>(
    systems: &Systems<N>,
    left: &[RingRow<N>],
    right: &[RingRow<N>],
) -> Diagnosis {
    let trace_left: Vec<TraceRow<N>> = left.iter().map(RingRow::trace).collect();
    let trace_right: Vec<TraceRow<N>> = right.iter().map(RingRow::trace).collect();
    let (mut canonical, compared) = join_rows(systems, &indexed(&trace_left), &indexed(&trace_right));
    // Every row at a compared tick counts (ADR-8724): a partial row anywhere at that tick.
    let partial_from = compared.iter().copied().find(|&tick| {
        left.iter()
            .chain(right)
            .any(|row| row.tick == tick && row.partial)
    });
    // A partial tick never counts as agreement (ADR-8621).
    canonical.last_agreed = agreed_before(canonical.last_agreed, partial_from, &compared);
    // The raw view joins every row that carries raw words, keeping its position in the input.
    let raw_rows = |rows: &[RingRow<N>]| -> Vec<(usize, TraceRow<N>)> {
        rows.iter()
            .enumerate()
            .filter_map(|(index, row)| {
                row.raw.map(|hashes| {
                    (
                        index,
                        TraceRow {
                            tick: row.tick,
                            hashes,
                        },
                    )
                })
            })
            .collect()
    };
    let (raw_left, raw_right) = (raw_rows(left), raw_rows(right));
    let raw = (!raw_left.is_empty() && !raw_right.is_empty())
        .then(|| join_rows(systems, &kept(&raw_left), &kept(&raw_right)))
        .filter(|(raw, _)| raw.compared > 0)
        .map(|(mut raw, raw_ticks)| {
            raw.last_agreed = agreed_before(raw.last_agreed, partial_from, &raw_ticks);
            raw
        });
    let mut float_bits = Vec::new();
    if let Some(raw) = &raw {
        for split in &raw.systems {
            let canonical_tick = canonical
                .systems
                .iter()
                .find(|d| d.system == split.system)
                .map(|d| d.first_tick);
            if canonical_tick.is_none_or(|t| split.first_tick < t) {
                float_bits.push(FloatBitsSplit {
                    system: split.system,
                    raw_tick: split.first_tick,
                    canonical_tick,
                });
            }
        }
    }
    Diagnosis {
        canonical,
        raw,
        float_bits,
        partial_from,
    }
}

/// The lines that name the split, then one line per [`FloatBitsSplit`] and one for a partial fold.
///
/// The first line names the view that disagreed ([`Diagnosis::verdict`], ADR-8621): on a canonical
/// split the first lines are exactly [`divergence_lines`]'s (`first <system> at tick <T>`); when only
/// the raw words split it says `raw words split first <system> at tick <T>`; when a compared row was
/// partial, `not agreed: partial rows from tick <T>`; with no shared tick, `not agreed: no shared
/// tick`. `no system differs` is printed only for [`Verdict::Same`]. When one side held more rows
/// than the other at a shared tick ([`DivergenceReport::surplus_ticks`]), a last line says so
/// (ADR-8724).
pub fn diagnosis_lines(
    split_tick: u64,
    left_label: &str,
    right_label: &str,
    diagnosis: &Diagnosis,
) -> Vec<String> {
    let report = &diagnosis.canonical;
    let head = format!("lockstep desync systems tick {split_tick} {left_label} vs {right_label}");
    let mut lines = match diagnosis.verdict() {
        Verdict::Canonical | Verdict::Same => divergence_lines(split_tick, left_label, right_label, report),
        Verdict::FloatBits => {
            let first = diagnosis.float_bits[0];
            vec![format!(
                "{head}: raw words split first {} at tick {} (zero sign or NaN payload; the canonical words agree in the {} shared ticks)",
                first.system, first.raw_tick, report.compared
            )]
        }
        Verdict::Partial => vec![format!(
            "{head}: not agreed: partial rows from tick {} (the words agree in the {} shared ticks, but a Serialize failed)",
            diagnosis.partial_from.unwrap_or(0),
            report.compared
        )],
        Verdict::NoOverlap => vec![format!("{head}: not agreed: no shared tick (0 compared)")],
    };
    for split in &diagnosis.float_bits {
        lines.push(format!(
            "lockstep desync raw {} tick {} {left_label} vs {right_label}: zero sign or NaN payload split (canonical {})",
            split.system,
            split.raw_tick,
            split
                .canonical_tick
                .map_or("never".to_string(), |t| format!("at tick {t}"))
        ));
    }
    if let Some(tick) = diagnosis.partial_from {
        lines.push(format!(
            "lockstep desync partial from tick {tick}: a Serialize failed while folding; those words cover part of the state"
        ));
    }
    let surplus = report.surplus_ticks();
    if let Some(first) = surplus.first() {
        let rows = |side: RowSide| {
            report
                .unmatched
                .iter()
                .filter(|row| row.tick_shared && row.side == side)
                .count()
        };
        lines.push(format!(
            "lockstep desync rows {left_label} vs {right_label}: more rows on one side at {} ticks from tick {first} ({} surplus {left_label}, {} surplus {right_label}); every row there was compared",
            surplus.len(),
            rows(RowSide::Left),
            rows(RowSide::Right)
        ));
    }
    lines
}

/// One trace row as a log line: `lockstep systems row <peer> <tick> <N hex words, ':'-joined>`.
pub fn row_line<const N: usize>(peer: &str, row: &TraceRow<N>) -> String {
    let mut words = String::with_capacity(N * 17);
    for (i, word) in row.hashes.words.iter().enumerate() {
        if i > 0 {
            words.push(':');
        }
        let _ = write!(words, "{word:016x}");
    }
    format!("lockstep systems row {peer} {} {words}", row.tick)
}

/// Parse a [`row_line`] (any prefix before `lockstep systems row` is ignored). `(peer, row)`;
/// `None` unless it holds exactly `N` words.
pub fn parse_row_line<const N: usize>(line: &str) -> Option<(String, TraceRow<N>)> {
    let rest = line.split("lockstep systems row ").nth(1)?;
    let mut parts = rest.split_whitespace();
    let peer = parts.next()?.to_string();
    let tick = parts.next()?.parse::<u64>().ok()?;
    let words = parts.next()?;
    let mut hashes = SystemHashes::<N>::default();
    let mut n = 0usize;
    for (i, word) in words.split(':').enumerate() {
        if i >= N {
            return None;
        }
        hashes.words[i] = u64::from_str_radix(word, 16).ok()?;
        n += 1;
    }
    (n == N).then_some((peer, TraceRow { tick, hashes }))
}

/// True on ticks where a sample period asks for a logged vector (`every` is the host's debug
/// setting; `None` or 0 means no samples). A checksum split still hashes once, via
/// [`SystemLogArm`].
pub fn should_sample(tick: u64, every: Option<u64>) -> bool {
    every.is_some_and(|n| n > 0 && tick.is_multiple_of(n))
}

/// What an injection request asks for (`<system>@<step>[#<seat>]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InjectPlan {
    /// The system to nudge.
    pub system: &'static str,
    /// The step label to nudge before.
    pub step: u64,
    /// Only this in-process seat index (0 is the host sim); `None` = whoever reads the plan.
    pub seat: Option<u32>,
}

/// Parse `<system>@<step>` or `<system>@<step>#<seat>` against `systems`. `None` for an unknown
/// system or a bad number.
pub fn parse_inject<const N: usize>(systems: &Systems<N>, text: &str) -> Option<InjectPlan> {
    let (name, rest) = text.trim().split_once('@')?;
    let (step, seat) = match rest.split_once('#') {
        Some((step, seat)) => (step, Some(seat.trim().parse::<u32>().ok()?)),
        None => (rest, None),
    };
    let i = systems.index_of(name.trim())?;
    Some(InjectPlan {
        system: systems.names[i],
        step: step.trim().parse::<u64>().ok()?,
        seat,
    })
}

/// A pending "log the vector for this tick" request, shared between the net thread that sees a
/// checksum split and the sim thread that hashes. The earliest tick wins until it is taken.
#[derive(Debug)]
pub struct SystemLogArm(AtomicU64);

impl Default for SystemLogArm {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemLogArm {
    /// Nothing armed.
    pub const fn new() -> Self {
        Self(AtomicU64::new(u64::MAX))
    }

    /// Ask the sim thread to log system hashes for `tick` ([`SYSTEM_LOG_TICK_UNKNOWN`] when the
    /// split tick is not known). The earliest tick wins until [`SystemLogArm::take`] consumes it.
    pub fn arm(&self, tick: u64) {
        let mut cur = self.0.load(Ordering::Relaxed);
        while cur == u64::MAX || tick < cur {
            match self
                .0
                .compare_exchange(cur, tick, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(seen) => cur = seen,
            }
        }
    }

    /// Take the pending tick, if a split armed one.
    pub fn take(&self) -> Option<u64> {
        let tick = self.0.swap(u64::MAX, Ordering::Relaxed);
        (tick != u64::MAX).then_some(tick)
    }
}
