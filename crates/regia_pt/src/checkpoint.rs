// SPDX-License-Identifier: MPL-2.0
//! Checkpointed, resumable bakes (ADR-3095): the lightmap bakers and the probe bake write what they
//! have finished to a `.bakeck` file ([`regia_asset::bake_checkpoint`]) every few batches, and a bake
//! started again with the same inputs continues from it.
//!
//! Determinism is the point. Every work unit (a lightmap texel, a probe of the main trace) keys its
//! random streams by its own index, never by its batch or thread, so a unit baked after a resume
//! gives the bits it would have given in an uninterrupted bake; the checkpoint stores the finished
//! units' sums bit for bit. A resumed bake's result is byte-identical to a bake that never stopped
//! (tested for the CPU lightmap baker and both probe tracers' paths on the CPU, and on lavapipe
//! for the GPU bakers).
//!
//! A checkpoint is only resumed by the bake that wrote it: its input hash covers the scene, the
//! layout, the receivers or probe subset, every setting that changes the result and the seed, and
//! the baker kind. Settings that do not change the result (threads, GPU tile size, memory budget,
//! the checkpoint's own cadence) are left out, so a resumed bake may use other ones.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub use regia_asset::bake_checkpoint::{
    BAKE_CHECKPOINT_DAMAGED, BAKE_CHECKPOINT_FORMAT_VERSION, BAKE_CHECKPOINT_IO, BAKE_CHECKPOINT_MISMATCH,
    BAKE_CHECKPOINT_VERSION, BakeCheckpoint, BakeCheckpointError, BakeCheckpointHeader, BakeCheckpointKind,
    BakeCheckpointStatus, bake_eta_seconds, checkpoint_status, default_checkpoint_path,
};

use crate::PtError;
use crate::sha256::Sha256;

/// The agent code of a bake stopped by [`BakeCheckpointPlan::stop_after_batches`] (the test hook
/// that stands for a crash): the checkpoint is written, nothing else.
pub const BAKE_INTERRUPTED: &str = "bake_interrupted";

/// Texels per batch of a checkpointed lightmap bake by default.
pub const DEFAULT_LIGHTMAP_BATCH_TEXELS: usize = 1 << 16;
/// Probes per batch of a checkpointed probe bake's main trace by default.
pub const DEFAULT_GI_BATCH_PROBES: usize = 4096;
/// The shortest time between two checkpoint writes by default.
pub const DEFAULT_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(60);

/// How a bake checkpoints: where, how often, and whether it resumes an existing file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BakeCheckpointPlan {
    /// The checkpoint file (`.bakeck`); written atomically (a temporary file beside it, renamed).
    pub path: PathBuf,
    /// Write after this many batches (at least 1).
    pub every_batches: u32,
    /// ... and no sooner than this after the previous write (or the start of the run); zero writes
    /// every `every_batches` batches.
    pub min_interval: Duration,
    /// Work units per batch: texels of a lightmap bake, probes of a probe bake's main trace (0 = the
    /// baker's default, [`DEFAULT_LIGHTMAP_BATCH_TEXELS`] or [`DEFAULT_GI_BATCH_PROBES`]).
    pub batch_units: usize,
    /// Continue from `path` when it exists (refused with `bake_checkpoint_mismatch` when another
    /// bake wrote it, `bake_checkpoint_damaged` when it does not read); `false` starts over and
    /// overwrites it.
    pub resume: bool,
    /// Test hook standing for a crash: after batch K (counted over every run of the bake) write
    /// the checkpoint and stop with `bake_interrupted`. `None` in every real bake.
    pub stop_after_batches: Option<u64>,
    /// Leave the checkpoint on disk when the bake finishes (`removed` stays `false`): a caller that
    /// still has to write the baked output removes it with [`remove_checkpoint`] once that write
    /// succeeded, so a failed write (disk full, a read-only output) keeps the bake resumable.
    pub keep_on_finish: bool,
}

impl BakeCheckpointPlan {
    /// A plan writing `path` every batch at most once a minute, resuming it when it exists.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            every_batches: 1,
            min_interval: DEFAULT_CHECKPOINT_INTERVAL,
            batch_units: 0,
            resume: true,
            stop_after_batches: None,
            keep_on_finish: false,
        }
    }

    /// `batch_units`, or `default` for 0.
    #[must_use]
    pub fn batch_or(&self, default: usize) -> usize {
        if self.batch_units == 0 {
            default
        } else {
            self.batch_units
        }
    }
}

/// What checkpointing did in a bake (the `checkpoint` object of the CLI's summary).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct BakeCheckpointStats {
    /// The checkpoint file.
    pub path: String,
    /// The bake continued a checkpoint.
    pub resumed: bool,
    /// Units the checkpoint had finished.
    pub resumed_units: u64,
    /// Batches the checkpoint had finished.
    pub resumed_batches: u64,
    /// Batches finished, over every run.
    pub batches: u64,
    /// Checkpoints written in this run.
    pub written: u32,
    /// The largest checkpoint written, bytes: the disk the checkpoint holds at most (twice that
    /// for a moment during the atomic rewrite).
    pub largest_bytes: u64,
    /// Seconds of baking over every run (the rate of the estimate).
    pub work_seconds: f64,
    /// The checkpoint was removed when the bake finished.
    pub removed: bool,
}

/// The checkpoint error code at the start of `error`'s message, if any: one of
/// `bake_checkpoint_mismatch`, `bake_checkpoint_damaged`, `bake_checkpoint_version`,
/// `bake_checkpoint_io` or `bake_interrupted`.
#[must_use]
pub fn bake_checkpoint_error_code(error: &PtError) -> Option<&'static str> {
    const CODES: [&str; 5] = [
        BAKE_CHECKPOINT_MISMATCH,
        BAKE_CHECKPOINT_DAMAGED,
        BAKE_CHECKPOINT_VERSION,
        BAKE_CHECKPOINT_IO,
        BAKE_INTERRUPTED,
    ];
    let message = match error {
        PtError::Scene(m) | PtError::Gpu(m) | PtError::Io(m) => m,
        PtError::DenoisedReference(_) => return None,
    };
    CODES
        .into_iter()
        .find(|c| message.strip_prefix(c).is_some_and(|rest| rest.starts_with(':')))
}

fn checkpoint_error(e: BakeCheckpointError) -> PtError {
    PtError::Io(e.to_string())
}

/// Where a bake is, for a progress line: the units done and in total over every run, and the
/// estimate of the rest from the rate so far ([`bake_eta_seconds`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BakeProgressEstimate {
    /// Units done (resumed ones included).
    pub done: u64,
    /// Units of the stage.
    pub total: u64,
    /// Seconds of baking over every run.
    pub work_seconds: f64,
}

impl BakeProgressEstimate {
    /// Units still to bake.
    #[must_use]
    pub fn remaining(&self) -> u64 {
        self.total.saturating_sub(self.done)
    }

    /// Seconds the rest takes at the rate so far.
    #[must_use]
    pub fn eta_seconds(&self) -> Option<f64> {
        bake_eta_seconds(self.done, self.total, self.work_seconds)
    }
}

/// The running checkpoint of one bake.
pub(crate) struct Checkpointer<'a> {
    plan: &'a BakeCheckpointPlan,
    kind: BakeCheckpointKind,
    input_hash: [u8; 32],
    /// Seconds of the runs before this one.
    prior_seconds: f64,
    started: Instant,
    last_write: Option<Instant>,
    since_write: u32,
    stats: BakeCheckpointStats,
}

impl<'a> Checkpointer<'a> {
    /// Opens the checkpoint of the bake `kind` with `input_hash`: the checkpoint to continue, when
    /// the plan resumes and the file exists.
    pub(crate) fn open(
        plan: &'a BakeCheckpointPlan,
        kind: BakeCheckpointKind,
        input_hash: [u8; 32],
    ) -> Result<(Self, Option<BakeCheckpoint>), PtError> {
        let mut me = Self {
            plan,
            kind,
            input_hash,
            prior_seconds: 0.0,
            started: Instant::now(),
            last_write: None,
            since_write: 0,
            stats: BakeCheckpointStats {
                path: plan.path.display().to_string(),
                ..BakeCheckpointStats::default()
            },
        };
        if !(plan.resume && plan.path.exists()) {
            return Ok((me, None));
        }
        let found = BakeCheckpoint::read(&plan.path).map_err(checkpoint_error)?;
        found.check_input(kind, &input_hash).map_err(checkpoint_error)?;
        me.prior_seconds = found.header.work_seconds;
        me.stats.resumed = true;
        me.stats.resumed_units = found.header.units_done;
        me.stats.resumed_batches = found.header.batches_done;
        me.stats.batches = found.header.batches_done;
        tracing::info!(
            target: "regia_pt",
            kind = kind.name(),
            units_done = found.header.units_done,
            units_total = found.header.units_total,
            "resuming a bake checkpoint"
        );
        Ok((me, Some(found)))
    }

    /// Seconds of baking over every run so far.
    pub(crate) fn work_seconds(&self) -> f64 {
        self.prior_seconds + self.started.elapsed().as_secs_f64()
    }

    /// Where the bake is, for progress.
    pub(crate) fn estimate(&self, done: u64, total: u64) -> BakeProgressEstimate {
        BakeProgressEstimate {
            done,
            total,
            work_seconds: self.work_seconds(),
        }
    }

    /// Counts a finished batch and writes the checkpoint when it is due (and not `last`, the batch
    /// that finishes the bake); after the plan's stop batch writes it and stops with
    /// `bake_interrupted`. `payload` encodes the baker's accumulators.
    pub(crate) fn batch_done(
        &mut self,
        stage: u32,
        units_total: u64,
        units_done: u64,
        last: bool,
        payload: impl FnOnce() -> Vec<u8>,
    ) -> Result<(), PtError> {
        self.stats.batches += 1;
        self.since_write += 1;
        let stop = self
            .plan
            .stop_after_batches
            .is_some_and(|k| self.stats.batches >= k);
        let since = self.last_write.unwrap_or(self.started).elapsed();
        let due = self.since_write >= self.plan.every_batches.max(1) && since >= self.plan.min_interval;
        if stop || (due && !last) {
            let _span = tracing::info_span!("bake_checkpoint_write", units_done).entered();
            let checkpoint = BakeCheckpoint {
                header: BakeCheckpointHeader {
                    kind: self.kind,
                    stage,
                    input_hash: self.input_hash,
                    units_total,
                    units_done,
                    batches_done: self.stats.batches,
                    work_seconds: self.work_seconds(),
                },
                payload: payload(),
            };
            let bytes = checkpoint
                .write_atomic(&self.plan.path)
                .map_err(checkpoint_error)?;
            self.stats.written += 1;
            self.stats.largest_bytes = self.stats.largest_bytes.max(bytes);
            self.last_write = Some(Instant::now());
            self.since_write = 0;
        }
        if stop {
            return Err(PtError::Io(format!(
                "{BAKE_INTERRUPTED}: stopped after batch {} by the test hook; the checkpoint {} holds {units_done} of {units_total} {}",
                self.stats.batches,
                self.plan.path.display(),
                self.kind.unit()
            )));
        }
        Ok(())
    }

    /// The bake finished: removes the checkpoint (and a stray temporary file) unless the plan keeps
    /// it (`keep_on_finish`), and returns the stats.
    pub(crate) fn finish(mut self) -> BakeCheckpointStats {
        self.stats.work_seconds = self.work_seconds();
        if !self.plan.keep_on_finish {
            self.stats.removed = remove_checkpoint(&self.plan.path);
        }
        self.stats
    }
}

/// Removes a finished bake's checkpoint at `path` and its stray temporary file. Returns `true` when
/// no checkpoint is left there (removed, or there was none).
pub fn remove_checkpoint(path: &std::path::Path) -> bool {
    let tmp = regia_asset::bake_checkpoint::temp_path(path);
    let _ = std::fs::remove_file(tmp);
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

/// A SHA-256 that `write!` streams `Debug` text into: the input hash of a bake without building
/// the text of a large scene in memory. `Debug` prints every float so it reads back exactly.
pub(crate) struct InputHasher(Sha256);

impl InputHasher {
    /// A hasher primed with `domain` (what is hashed and the layout version).
    pub(crate) fn new(domain: &str) -> Self {
        let mut h = Self(Sha256::new());
        h.bytes(domain.as_bytes());
        h
    }

    /// Feeds raw bytes, length-prefixed.
    pub(crate) fn bytes(&mut self, b: &[u8]) {
        self.0.update(&(b.len() as u64).to_le_bytes());
        self.0.update(b);
    }

    /// Feeds `value`'s `Debug` text, then a separator.
    pub(crate) fn debug(&mut self, value: &dyn std::fmt::Debug) {
        let _ = write!(self, "{value:?}");
        self.0.update(&[0xff]);
    }

    /// The digest.
    pub(crate) fn finish(self) -> [u8; 32] {
        self.0.finish()
    }
}

impl std::fmt::Write for InputHasher {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.update(s.as_bytes());
        Ok(())
    }
}

/// Little-endian payload writer.
#[derive(Default)]
pub(crate) struct Enc(pub(crate) Vec<u8>);

impl Enc {
    pub(crate) fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    pub(crate) fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn f32(&mut self, v: f32) {
        self.u32(v.to_bits());
    }
    pub(crate) fn f64(&mut self, v: f64) {
        self.u64(v.to_bits());
    }
}

/// Bounds-checked little-endian payload reader: every read past the end is
/// `bake_checkpoint_damaged`, never a panic.
pub(crate) struct Dec<'b> {
    b: &'b [u8],
    at: usize,
}

impl<'b> Dec<'b> {
    pub(crate) fn new(b: &'b [u8]) -> Self {
        Self { b, at: 0 }
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], PtError> {
        let end = self.at.checked_add(N).filter(|&e| e <= self.b.len());
        let Some(end) = end else {
            return Err(damaged(format!(
                "the payload ends at byte {} of a field",
                self.b.len()
            )));
        };
        let mut a = [0u8; N];
        a.copy_from_slice(&self.b[self.at..end]);
        self.at = end;
        Ok(a)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, PtError> {
        self.take::<1>().map(|a| a[0])
    }
    pub(crate) fn u32(&mut self) -> Result<u32, PtError> {
        self.take::<4>().map(u32::from_le_bytes)
    }
    pub(crate) fn u64(&mut self) -> Result<u64, PtError> {
        self.take::<8>().map(u64::from_le_bytes)
    }
    pub(crate) fn f32(&mut self) -> Result<f32, PtError> {
        self.u32().map(f32::from_bits)
    }
    pub(crate) fn f64(&mut self) -> Result<f64, PtError> {
        self.u64().map(f64::from_bits)
    }

    /// A count that must be at most `most` and fit `item_bytes` each in the rest of the payload.
    pub(crate) fn count(&mut self, most: u64, item_bytes: usize) -> Result<usize, PtError> {
        let n = self.u64()?;
        let left = (self.b.len() - self.at) as u64;
        if n > most || n.saturating_mul(item_bytes as u64) > left {
            return Err(damaged(format!(
                "a count of {n} (at most {most}, {left} bytes left)"
            )));
        }
        Ok(n as usize)
    }

    /// The payload was read to its end.
    pub(crate) fn end(&self) -> Result<(), PtError> {
        if self.at == self.b.len() {
            Ok(())
        } else {
            Err(damaged(format!(
                "{} bytes after the payload's last field",
                self.b.len() - self.at
            )))
        }
    }
}

/// A `bake_checkpoint_damaged` error.
pub(crate) fn damaged(message: impl std::fmt::Display) -> PtError {
    PtError::Io(format!("{BAKE_CHECKPOINT_DAMAGED}: {message}"))
}

/// What a probe bake has finished (the `gi` payload): the relocation's outcome over the bake's
/// `n` probes and the main trace's first `traces.len()` of `traced` probes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GiCheckpointState {
    /// Relocation offset per probe.
    pub offsets: Vec<[f32; 3]>,
    /// Flags per probe after relocation.
    pub flags: Vec<u16>,
    /// (back-face fraction, rays) of each probe's last relocation trace at its final position.
    pub last_trace: Vec<Option<(f32, u32)>>,
    /// Relocation and classification traces run.
    pub relocation_traces: u64,
    /// Seconds the relocation took (for the report).
    pub relocation_seconds: f64,
    /// Probes of the main trace.
    pub traced: u64,
    /// The main trace's results so far, in trace order.
    pub traces: Vec<crate::probe::ProbeTrace>,
}

fn opt_dir(e: &mut Enc, v: Option<([f32; 3], f32)>) {
    let (tag, [x, y, z], d) = v.map_or((0, [0.0; 3], 0.0), |(dir, d)| (1, dir, d));
    e.u8(tag);
    for f in [x, y, z, d] {
        e.f32(f);
    }
}

fn read_opt_dir(d: &mut Dec<'_>) -> Result<Option<([f32; 3], f32)>, PtError> {
    let tag = d.u8()?;
    let v = [d.f32()?, d.f32()?, d.f32()?, d.f32()?];
    match tag {
        0 => Ok(None),
        1 => Ok(Some(([v[0], v[1], v[2]], v[3]))),
        t => Err(damaged(format!("option tag {t}"))),
    }
}

/// Bytes of one probe's relocation record (offset, flags, last trace).
const GI_PROBE_BYTES: usize = 12 + 4 + 9;
/// Fewest bytes of one trace record.
const GI_TRACE_MIN_BYTES: usize = 48 * 4 + 16 + 17 + 17 + 4 + 16 + 5;

/// Encodes a probe bake's state.
pub(crate) fn encode_gi(
    offsets: &[[f32; 3]],
    flags: &[u16],
    last_trace: &[Option<(f32, u32)>],
    relocation_traces: u64,
    relocation_seconds: f64,
    traced: u64,
    traces: &[crate::probe::ProbeTrace],
) -> Vec<u8> {
    let mut e = Enc::default();
    e.u64(offsets.len() as u64);
    for ((o, &f), l) in offsets.iter().zip(flags).zip(last_trace) {
        for v in o {
            e.f32(*v);
        }
        e.u32(u32::from(f));
        let (tag, frac, rays) = l.map_or((0, 0.0, 0), |(f, r)| (1, f, r));
        e.u8(tag);
        e.f32(frac);
        e.u32(rays);
    }
    e.u64(relocation_traces);
    e.f64(relocation_seconds);
    e.u64(traced);
    e.u64(traces.len() as u64);
    for t in traces {
        for v in t.sh.iter().flatten().flatten() {
            e.f32(*v);
        }
        for v in [t.rays, t.back_faces, t.hits, t.rejected] {
            e.u32(v);
        }
        opt_dir(&mut e, t.closest_back);
        opt_dir(&mut e, t.closest_front);
        e.u32(t.moments.len() as u32);
        for m in &t.moments {
            e.f32(m[0]);
            e.f32(m[1]);
        }
        e.u32(t.clamped);
        e.f32(t.clamp_loss);
        e.u32(t.suppressed_groups);
        e.f32(t.firefly_loss);
        e.u8(u8::from(t.noise.is_some()));
        e.f32(t.noise.unwrap_or(0.0));
    }
    e.0
}

/// Decodes a probe bake's state for a bake of `n` probes whose traces have `texels` moment texels,
/// with `done` traces finished (the header's count).
pub(crate) fn decode_gi(
    payload: &[u8],
    n: usize,
    texels: usize,
    done: u64,
) -> Result<GiCheckpointState, PtError> {
    let mut d = Dec::new(payload);
    let count = d.count(n as u64, GI_PROBE_BYTES)?;
    if count != n {
        return Err(damaged(format!(
            "the checkpoint holds {count} probes, the bake {n}"
        )));
    }
    let mut s = GiCheckpointState {
        offsets: Vec::with_capacity(n),
        flags: Vec::with_capacity(n),
        last_trace: Vec::with_capacity(n),
        relocation_traces: 0,
        relocation_seconds: 0.0,
        traced: 0,
        traces: Vec::new(),
    };
    for _ in 0..n {
        s.offsets.push([d.f32()?, d.f32()?, d.f32()?]);
        let f = d.u32()?;
        s.flags
            .push(u16::try_from(f).map_err(|_| damaged(format!("probe flags {f}")))?);
        let (tag, frac, rays) = (d.u8()?, d.f32()?, d.u32()?);
        s.last_trace.push(match tag {
            0 => None,
            1 => Some((frac, rays)),
            t => return Err(damaged(format!("option tag {t}"))),
        });
    }
    s.relocation_traces = d.u64()?;
    s.relocation_seconds = d.f64()?;
    s.traced = d.u64()?;
    if s.traced > n as u64 {
        return Err(damaged(format!("{} traced probes of {n}", s.traced)));
    }
    let traces = d.count(s.traced, GI_TRACE_MIN_BYTES)?;
    if traces as u64 != done {
        return Err(damaged(format!(
            "the payload holds {traces} traces, the header {done}"
        )));
    }
    s.traces.reserve(traces);
    for _ in 0..traces {
        let mut t = crate::probe::ProbeTrace::default();
        for v in t.sh.iter_mut().flatten().flatten() {
            *v = d.f32()?;
        }
        t.rays = d.u32()?;
        t.back_faces = d.u32()?;
        t.hits = d.u32()?;
        t.rejected = d.u32()?;
        t.closest_back = read_opt_dir(&mut d)?;
        t.closest_front = read_opt_dir(&mut d)?;
        let m = d.u32()? as usize;
        if m != texels {
            return Err(damaged(format!(
                "a trace of {m} moment texels, the bake {texels}"
            )));
        }
        t.moments = (0..m)
            .map(|_| Ok([d.f32()?, d.f32()?]))
            .collect::<Result<_, PtError>>()?;
        t.clamped = d.u32()?;
        t.clamp_loss = d.f32()?;
        t.suppressed_groups = d.u32()?;
        t.firefly_loss = d.f32()?;
        let (tag, noise) = (d.u8()?, d.f32()?);
        t.noise = match tag {
            0 => None,
            1 => Some(noise),
            t => return Err(damaged(format!("option tag {t}"))),
        };
        s.traces.push(t);
    }
    d.end()?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gi_payload_round_trips_bit_for_bit_and_refuses_wrong_shapes() {
        let mut t = crate::probe::ProbeTrace {
            rays: 64,
            back_faces: 3,
            hits: 50,
            closest_back: Some(([0.0, 1.0, 0.0], 0.25)),
            moments: vec![[1.0, 1.5]; 4],
            noise: Some(f32::from_bits(0x7fc0_0001)),
            ..crate::probe::ProbeTrace::default()
        };
        t.sh[1][2][3] = -0.125;
        let bytes = encode_gi(
            &[[0.1, 0.0, -0.2], [0.0; 3]],
            &[3, 1],
            &[Some((0.5, 64)), None],
            7,
            1.5,
            2,
            std::slice::from_ref(&t),
        );
        let s = decode_gi(&bytes, 2, 4, 1).expect("decodes");
        assert_eq!(s.flags, vec![3, 1]);
        assert_eq!(s.last_trace, vec![Some((0.5, 64)), None]);
        assert_eq!((s.relocation_traces, s.traced), (7, 2));
        assert_eq!(s.traces[0].sh[1][2][3], -0.125);
        assert_eq!(
            s.traces[0].noise.map(f32::to_bits),
            Some(0x7fc0_0001),
            "NaN bits kept"
        );
        assert_eq!(
            encode_gi(&s.offsets, &s.flags, &s.last_trace, 7, 1.5, 2, &s.traces),
            bytes
        );
        assert!(decode_gi(&bytes, 3, 4, 1).is_err(), "another probe count");
        assert!(decode_gi(&bytes, 2, 9, 1).is_err(), "another moment resolution");
        assert!(decode_gi(&bytes, 2, 4, 0).is_err(), "the header disagrees");
        for cut in [0, 10, bytes.len() / 2, bytes.len() - 1] {
            assert!(decode_gi(&bytes[..cut], 2, 4, 1).is_err(), "cut {cut}");
        }
    }

    /// Seeded mutation of a real payload (flips, truncations, huge counts): the decoder returns an
    /// error or a state, never panics, and never allocates past the payload.
    #[test]
    fn mutated_gi_payloads_never_panic() {
        let t = crate::probe::ProbeTrace {
            moments: vec![[1.0, 1.0]; 4],
            ..crate::probe::ProbeTrace::default()
        };
        let good = encode_gi(&[[0.0; 3]; 3], &[1, 1, 1], &[None; 3], 1, 0.5, 3, &[t.clone(), t]);
        let mut x = 0x9e37_79b9_u32;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        for _ in 0..2000 {
            let mut b = good.clone();
            match next() % 4 {
                0 => {
                    let i = next() as usize % b.len();
                    b[i] ^= 1 << (next() % 8);
                }
                1 => b.truncate(next() as usize % b.len()),
                2 => {
                    let i = (next() as usize % (b.len() - 8)) & !7;
                    b[i..i + 8].copy_from_slice(&u64::MAX.to_le_bytes());
                }
                _ => b.extend(std::iter::repeat_n(0xff, next() as usize % 64)),
            }
            let _ = decode_gi(&b, 3, 4, 2);
        }
    }

    #[test]
    fn error_codes_are_read_from_the_message_start() {
        let e = PtError::Io(format!("{BAKE_CHECKPOINT_MISMATCH}: other bake"));
        assert_eq!(bake_checkpoint_error_code(&e), Some(BAKE_CHECKPOINT_MISMATCH));
        let e = PtError::Io(format!("{BAKE_INTERRUPTED}: hook"));
        assert_eq!(bake_checkpoint_error_code(&e), Some(BAKE_INTERRUPTED));
        assert_eq!(bake_checkpoint_error_code(&PtError::Scene("no".into())), None);
        assert_eq!(
            bake_checkpoint_error_code(&PtError::Io("bake_checkpoint_iox: no".into())),
            None
        );
    }

    #[test]
    fn the_decoder_refuses_short_payloads_and_huge_counts() {
        let mut e = Enc::default();
        e.u64(3);
        e.u32(1);
        let mut d = Dec::new(&e.0);
        assert!(d.count(10, 4).is_err(), "3 items of 4 bytes do not fit in 4");
        let mut d = Dec::new(&e.0);
        assert_eq!(d.u64().ok(), Some(3));
        assert_eq!(d.u32().ok(), Some(1));
        assert!(d.u8().is_err());
        assert!(d.end().is_ok());
        let mut e = Enc::default();
        e.u64(u64::MAX);
        assert!(Dec::new(&e.0).count(u64::MAX, 1).is_err());
    }

    /// The cadence: every N batches, never on the last one, always at the stop batch.
    #[test]
    fn checkpoints_are_written_every_n_batches_and_at_the_stop_batch() {
        let dir = regia_test_utils::unique_temp_dir("pt_checkpoint_cadence");
        let mut plan = BakeCheckpointPlan::new(dir.join(".x.bakeck"));
        plan.every_batches = 2;
        plan.min_interval = Duration::ZERO;
        plan.stop_after_batches = Some(5);
        let (mut ck, found) =
            Checkpointer::open(&plan, BakeCheckpointKind::LightmapCpu, [1; 32]).expect("open");
        assert!(found.is_none());
        for b in 1..=4u64 {
            ck.batch_done(0, 10, b, false, || vec![b as u8]).expect("batch");
        }
        assert_eq!(ck.stats.written, 2);
        let e = ck.batch_done(0, 10, 5, false, || vec![5]).expect_err("stops");
        assert_eq!(bake_checkpoint_error_code(&e), Some(BAKE_INTERRUPTED));
        assert_eq!(ck.stats.written, 3);
        let read = BakeCheckpoint::read(&plan.path).expect("reads");
        assert_eq!((read.header.units_done, read.header.batches_done), (5, 5));
        // Resuming counts on from the checkpoint's batches; another hash is refused.
        plan.stop_after_batches = None;
        let (ck, found) =
            Checkpointer::open(&plan, BakeCheckpointKind::LightmapCpu, [1; 32]).expect("resume");
        assert_eq!(found.map(|c| c.payload), Some(vec![5]));
        assert_eq!(ck.stats.batches, 5);
        let stats = ck.finish();
        assert!(stats.removed && !plan.path.exists());
        BakeCheckpoint {
            header: read.header.clone(),
            payload: vec![],
        }
        .write_atomic(&plan.path)
        .expect("write");
        let e = Checkpointer::open(&plan, BakeCheckpointKind::LightmapCpu, [2; 32])
            .err()
            .expect("mismatch");
        assert_eq!(bake_checkpoint_error_code(&e), Some(BAKE_CHECKPOINT_MISMATCH));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `keep_on_finish` leaves the checkpoint for the caller, which removes it once the output
    /// is written ([`remove_checkpoint`]).
    #[test]
    fn audit_a_kept_checkpoint_survives_finish_until_removed() {
        let dir = regia_test_utils::unique_temp_dir("pt_checkpoint_keep");
        let mut plan = BakeCheckpointPlan::new(dir.join(".x.bakeck"));
        plan.min_interval = Duration::ZERO;
        plan.keep_on_finish = true;
        let (mut ck, _) = Checkpointer::open(&plan, BakeCheckpointKind::LightmapCpu, [1; 32]).expect("open");
        ck.batch_done(0, 10, 5, false, || vec![5]).expect("batch");
        let stats = ck.finish();
        assert!(!stats.removed && plan.path.is_file(), "kept for the caller");
        assert!(remove_checkpoint(&plan.path));
        assert!(!plan.path.exists());
        assert!(remove_checkpoint(&plan.path), "nothing left counts as removed");
        let _ = std::fs::remove_dir_all(dir);
    }
}
