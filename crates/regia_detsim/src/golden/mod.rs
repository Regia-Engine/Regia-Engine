// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/trace_harness.rs, commit d6ed37677), ADR-8012.
// Mark: adapt. Taken: the shape of DO27's libm-1000 check (run fixed inputs for N ticks, fingerprint the
// f32 bit patterns of the state, compare with a committed `key=value` hash line) and `resample_zoh`
// (zero-order hold of an input trace onto sim ticks). Named issues, each a change:
//   1. One committed final hash says only *that* a run drifted. A golden now keeps one hash per tick of
//      the reference target, so a report names the first diverging tick (plan row F-02).
//   2. The fingerprint was a hex string of six named fields. It is an FNV-1a 64 over every hashed word,
//      with NaNs folded to one pattern (x86-64 and aarch64 produce different NaN payloads), and a field
//      log on demand so a live comparison names the fields that moved.
//   3. The hover step, its parameters and the fixture paths belong to the game (plan row S-07); the
//      scenario is the `DetScenario` trait, and a game registers its own.
//   4. `resample_zoh` takes any sample type with a time accessor and returns no ticks for an empty trace
//      instead of asserting.
//   5. One committed hash per target (`hash.<arch>-<os>=`), not one for every OS.

//! Deterministic golden harness: seed, N ticks, a hash per tick, a committed hash per target
//! (ADR-8046, plan row F-02).
//!
//! A [`DetScenario`] is a lockstep simulation in miniature: [`DetScenario::reset`] from a
//! [`DetRun`] (seed, tick count, tick rate), [`DetScenario::step`] once per tick, and
//! [`DetScenario::hash_state`] feeding every state word into a [`StateHasher`]. [`record`] runs it
//! and keeps the hash after every tick in a [`DetTrace`].
//!
//! Two questions use a trace:
//!
//! - **Did this build compute the committed bits?** [`check_golden`] compares a trace with the
//!   scenario's golden file (`<name>.detgold`, [`Golden`]). The golden keeps a run hash for each
//!   target that has been checked (`x86_64-linux`, `aarch64-macos`, `aarch64-android`, ...) and the
//!   per-tick hashes of one reference target. A target with no line of its own must equal the
//!   reference (the lockstep policy is bit identity across architectures, ADR-8012 A-4). The
//!   [`DetReport`] names the first tick whose hash differs from the reference trace.
//! - **Where did two live runs part?** [`compare`] records two scenarios (optionally with an
//!   [`Injection`], one flipped state bit) and returns the first diverging tick with the fields that
//!   differ there.
//!
//! [`rate_gate`] holds the 60 Hz vs 120 Hz agreement gates, and [`scenarios`] the engine's own
//! scenarios (steering, slope following, disc collisions, the AI arena driven by a recorded input
//! log, and [`vectors`]: the extended math, the RNG and the integer helpers over a fixed table). A
//! game registers its scenarios in a [`DetRegistry`] beside them, with its own golden directory.
//! [`InputLogScenario`] turns any recorded run (a start snapshot and a [`crate::replay::InputLog`])
//! into a golden scenario (DC-3.7b).
//!
//! Blessing: with `REGIA_BLESS=1` in the environment ([`GoldenMode::from_env`]) a check writes the
//! current target's hash (and the per-tick trace when the file is new or this target is the
//! reference) instead of comparing. Bless only when the change in bits is intended, and say why in
//! the commit message.
//!
//! ```
//! use regia_detsim::golden::{self, DetScenario, Injection, scenarios::SteerScenario};
//!
//! let mut clean = SteerScenario::new();
//! let mut flipped = SteerScenario::new();
//! let mut run = clean.default_run();
//! run.ticks = 120;
//! let injection = Injection { tick: 40, bit: 0 };
//! let divergence = golden::compare(&mut clean, &mut flipped, &run, Some(injection))
//!     .expect("the steering scenario supports injection")
//!     .expect("a flipped bit diverges");
//! assert_eq!(divergence.tick, 40);
//! assert_eq!(divergence.fields[0].name, "car[0].pos_x");
//! ```

use std::fmt;
use std::path::{Path, PathBuf};

/// The tick rate of a [`DetRun`], re-exported for scenario authors.
pub use crate::tick::TickRate;

pub mod rate_gate;
pub mod scenarios;
pub mod vectors;

mod input_log;
pub use input_log::InputLogScenario;

#[cfg(test)]
mod fp_env_tests;

/// The `format=` value of a golden file.
pub const FORMAT: &str = "regia-det-golden-1";

/// File extension of a golden file (`<scenario>.detgold`).
pub const EXTENSION: &str = "detgold";

/// Environment variable that turns a check into a bless when set to `1` (the repository convention).
pub const BLESS_ENV: &str = "REGIA_BLESS";

/// Environment variable that points [`engine_golden_dir`] somewhere else (a runner that ships the
/// goldens next to a cross-built test binary).
pub const GOLDEN_DIR_ENV: &str = "REGIA_DET_GOLDEN_DIR";

/// Prefix of the one-line JSON report a check prints ([`DetReport::to_json`]); runners grep for it.
pub const REPORT_PREFIX: &str = "regia-det-golden ";

/// The target whose per-tick trace a new golden keeps when nothing else is recorded.
pub const REFERENCE_TARGET: &str = "x86_64-linux";

/// Longest scenario name.
pub const MAX_NAME_LEN: usize = 64;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// One run of a scenario: the seed, the number of ticks and the tick rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetRun {
    /// Seed of the scenario's random inputs.
    pub seed: u64,
    /// Ticks to step; the trace keeps one hash per tick.
    pub ticks: u32,
    /// Tick rate; `dt = rate.dt()`.
    pub rate: TickRate,
}

impl DetRun {
    /// A run of `ticks` ticks at `rate` from `seed`.
    pub const fn new(seed: u64, ticks: u32, rate: TickRate) -> Self {
        DetRun { seed, ticks, rate }
    }
}

/// A deterministic simulation the harness can step and hash.
///
/// Rules for an implementation (the same as for lockstep code, ADR-8012 A-4): transcendentals
/// through [`crate::math`], no hash-container iteration, no SIMD vector types, no threads, inputs
/// only from the seed. [`DetScenario::hash_state`] must feed every word that a later tick reads, in
/// a fixed order.
pub trait DetScenario {
    /// Stable name: lowercase ASCII letters, digits and `_`, at most [`MAX_NAME_LEN`] bytes. It is
    /// the golden file's stem.
    fn name(&self) -> &str;

    /// Whether this scenario deliberately plants a cross-target divergence (default: false).
    /// Only diagnostic negative controls should return `true`; ordinary scenarios must agree
    /// with their golden's reference even after a per-target hash has been blessed.
    fn planted(&self) -> bool {
        false
    }

    /// The run the committed golden is recorded with.
    fn default_run(&self) -> DetRun;

    /// Put the scenario in its start state for `run`.
    fn reset(&mut self, run: &DetRun);

    /// Advance one tick. `tick` counts from 0.
    fn step(&mut self, tick: u32);

    /// Feed the state into `h`, in a fixed order. Name every field; [`StateHasher::item`] scopes
    /// the names that follow it (`car[2].yaw`).
    fn hash_state(&self, h: &mut StateHasher);

    /// Flip bit `bit` (0-31) of the scenario's first hashed field, for fault injection. Return
    /// `false` if the scenario does not support it (the default).
    fn flip_bit(&mut self, bit: u32) -> bool {
        let _ = bit;
        false
    }
}

/// The value of one hashed field, kept by a logging [`StateHasher`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldValue {
    /// `item[index].field`, or `field` outside an item.
    pub name: String,
    /// The bits that were hashed (an `f32` with NaNs folded, see [`canonical_f32_bits`]).
    pub bits: u64,
}

/// FNV-1a 64 over little-endian state words, with an optional field log.
#[derive(Debug, Clone)]
pub struct StateHasher {
    hash: u64,
    item: Option<(&'static str, u32)>,
    log: Option<Vec<FieldValue>>,
}

impl Default for StateHasher {
    fn default() -> Self {
        StateHasher::new()
    }
}

impl StateHasher {
    /// A hasher without a field log (the per-tick path; no allocation).
    pub const fn new() -> Self {
        StateHasher {
            hash: FNV_OFFSET,
            item: None,
            log: None,
        }
    }

    /// A hasher that also logs every field's name and bits (for divergence reports).
    pub fn with_log() -> Self {
        StateHasher {
            hash: FNV_OFFSET,
            item: None,
            log: Some(Vec::new()),
        }
    }

    /// The hash so far.
    pub const fn finish(&self) -> u64 {
        self.hash
    }

    /// The logged fields (empty for a hasher made with [`StateHasher::new`]).
    pub fn fields(&self) -> &[FieldValue] {
        self.log.as_deref().unwrap_or(&[])
    }

    /// Take the logged fields.
    pub fn into_fields(self) -> Vec<FieldValue> {
        self.log.unwrap_or_default()
    }

    /// Start item `index` of kind `kind`: the index is hashed, and logged field names that follow
    /// read `kind[index].field`.
    pub fn item(&mut self, kind: &'static str, index: u32) {
        self.bytes(&index.to_le_bytes());
        self.item = Some((kind, index));
    }

    /// Hash an `f32` by its bits, NaNs folded to one pattern.
    pub fn f32(&mut self, name: &'static str, v: f32) {
        let bits = canonical_f32_bits(v);
        self.bytes(&bits.to_le_bytes());
        self.note(name, u64::from(bits));
    }

    /// Hash a `u32`.
    pub fn u32(&mut self, name: &'static str, v: u32) {
        self.bytes(&v.to_le_bytes());
        self.note(name, u64::from(v));
    }

    /// Hash an `i32`.
    pub fn i32(&mut self, name: &'static str, v: i32) {
        self.bytes(&v.to_le_bytes());
        self.note(name, u64::from(v as u32));
    }

    /// Hash a `u64`.
    pub fn u64(&mut self, name: &'static str, v: u64) {
        self.bytes(&v.to_le_bytes());
        self.note(name, v);
    }

    /// Hash a `bool` as one byte.
    pub fn bool(&mut self, name: &'static str, v: bool) {
        self.bytes(&[u8::from(v)]);
        self.note(name, u64::from(v));
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.hash ^= u64::from(b);
            self.hash = self.hash.wrapping_mul(FNV_PRIME);
        }
    }

    fn note(&mut self, name: &'static str, bits: u64) {
        let item = self.item;
        if let Some(log) = self.log.as_mut() {
            let name = match item {
                Some((kind, index)) => format!("{kind}[{index}].{name}"),
                None => name.to_string(),
            };
            log.push(FieldValue { name, bits });
        }
    }
}

/// The bits an `f32` is hashed with: its own bits, except that every NaN becomes `0x7fc0_0000`.
/// Hardware NaN payloads and signs differ between x86-64 and aarch64, so a cross-target hash must
/// not see them. `-0.0` and `0.0` stay distinct.
pub const fn canonical_f32_bits(v: f32) -> u32 {
    if v.is_nan() { 0x7fc0_0000 } else { v.to_bits() }
}

/// The hash after every tick of one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetTrace {
    /// The scenario's name.
    pub scenario: String,
    /// The run that was recorded.
    pub run: DetRun,
    /// `tick_hashes[t]` is the state hash after tick `t` stepped.
    pub tick_hashes: Vec<u64>,
    /// Whether the recording scenario explicitly permits planted cross-target differences.
    /// Copied from [`DetScenario::planted`]; set to `false` for ordinary hand-built traces.
    /// This policy is not hashed or serialized into the golden file.
    pub planted: bool,
}

impl DetTrace {
    /// One hash for the whole run: FNV-1a 64 over the tick count and every tick hash, so it moves if
    /// any tick moves.
    pub fn run_hash(&self) -> u64 {
        run_hash(&self.tick_hashes)
    }
}

/// [`DetTrace::run_hash`] of a list of tick hashes.
pub fn run_hash(tick_hashes: &[u64]) -> u64 {
    let mut h = StateHasher::new();
    h.u64("ticks", tick_hashes.len() as u64);
    for &t in tick_hashes {
        h.u64("tick", t);
    }
    h.finish()
}

/// The first tick at which two hash lists differ. A list that is a prefix of the other diverges at
/// the first tick past its end; equal lists give `None`.
pub fn first_divergence(expected: &[u64], actual: &[u64]) -> Option<u32> {
    let common = expected.len().min(actual.len());
    let index = expected
        .iter()
        .zip(actual)
        .position(|(e, a)| e != a)
        .or((expected.len() != actual.len()).then_some(common))?;
    Some(u32::try_from(index).unwrap_or(u32::MAX))
}

/// Fault injection: flip one state bit right after a tick steps (before it is hashed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Injection {
    /// The tick after whose step the bit flips.
    pub tick: u32,
    /// The bit (0-31) of the scenario's first hashed field.
    pub bit: u32,
}

/// An error from the harness. [`DetError::code`] is the stable code a report carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetError {
    /// A scenario name is empty, too long, or has a byte other than `a-z`, `0-9`, `_`.
    BadScenarioName(String),
    /// Two registered scenarios share a name.
    DuplicateScenario(String),
    /// The scenario's [`DetScenario::flip_bit`] returned `false`.
    InjectionUnsupported(String),
    /// A golden file does not parse.
    GoldenInvalid(String),
}

impl DetError {
    /// The stable, lowercase error code.
    pub const fn code(&self) -> &'static str {
        match self {
            DetError::BadScenarioName(_) => "bad_scenario_name",
            DetError::DuplicateScenario(_) => "duplicate_scenario",
            DetError::InjectionUnsupported(_) => "injection_unsupported",
            DetError::GoldenInvalid(_) => "golden_invalid",
        }
    }
}

impl fmt::Display for DetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DetError::BadScenarioName(name) => write!(
                f,
                "{}: scenario name {name:?} must be 1-{MAX_NAME_LEN} bytes of a-z, 0-9 and _",
                self.code()
            ),
            DetError::DuplicateScenario(name) => {
                write!(f, "{}: scenario {name:?} is registered twice", self.code())
            }
            DetError::InjectionUnsupported(name) => {
                write!(f, "{}: scenario {name:?} cannot flip a state bit", self.code())
            }
            DetError::GoldenInvalid(why) => write!(f, "{}: {why}", self.code()),
        }
    }
}

impl std::error::Error for DetError {}

/// Check a scenario name (see [`DetScenario::name`]).
pub fn validate_name(name: &str) -> Result<(), DetError> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(DetError::BadScenarioName(name.to_string()))
    }
}

/// Run `scenario` for `run` and keep the hash after every tick. The calling thread's floating-point
/// environment is put back to the IEEE default first ([`crate::fpu::ensure_default_fp_env`]), so a
/// thread that inherited flush-to-zero records the same trace as a clean one.
pub fn record(scenario: &mut dyn DetScenario, run: &DetRun) -> DetTrace {
    // Float-policy F4 (ADR-10062): the run starts on the IEEE default environment, whatever the
    // calling thread inherited or a previous test left there.
    crate::fpu::ensure_default_fp_env();
    let mut tick_hashes = Vec::with_capacity(run.ticks as usize);
    scenario.reset(run);
    for tick in 0..run.ticks {
        scenario.step(tick);
        let mut h = StateHasher::new();
        scenario.hash_state(&mut h);
        tick_hashes.push(h.finish());
    }
    DetTrace {
        scenario: scenario.name().to_string(),
        run: *run,
        tick_hashes,
        planted: scenario.planted(),
    }
}

/// [`record`] with an optional [`Injection`].
pub fn record_injected(
    scenario: &mut dyn DetScenario,
    run: &DetRun,
    injection: Option<Injection>,
) -> Result<DetTrace, DetError> {
    let Some(injection) = injection else {
        return Ok(record(scenario, run));
    };
    crate::fpu::ensure_default_fp_env();
    let mut tick_hashes = Vec::with_capacity(run.ticks as usize);
    scenario.reset(run);
    for tick in 0..run.ticks {
        scenario.step(tick);
        if tick == injection.tick && !scenario.flip_bit(injection.bit) {
            return Err(DetError::InjectionUnsupported(scenario.name().to_string()));
        }
        let mut h = StateHasher::new();
        scenario.hash_state(&mut h);
        tick_hashes.push(h.finish());
    }
    Ok(DetTrace {
        scenario: scenario.name().to_string(),
        run: *run,
        tick_hashes,
        planted: scenario.planted(),
    })
}

/// The logged fields of `scenario` after tick `tick` of `run` (with `injection` applied if it falls
/// at or before `tick`).
pub fn fields_at(
    scenario: &mut dyn DetScenario,
    run: &DetRun,
    tick: u32,
    injection: Option<Injection>,
) -> Result<Vec<FieldValue>, DetError> {
    scenario.reset(run);
    for t in 0..=tick.min(run.ticks.saturating_sub(1)) {
        scenario.step(t);
        if let Some(inj) = injection
            && inj.tick == t
            && !scenario.flip_bit(inj.bit)
        {
            return Err(DetError::InjectionUnsupported(scenario.name().to_string()));
        }
    }
    let mut h = StateHasher::with_log();
    scenario.hash_state(&mut h);
    Ok(h.into_fields())
}

/// One field that differs at the diverging tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDiff {
    /// The field's logged name (`car[0].pos_x`).
    pub name: String,
    /// Its bits in the expected run (`None` if that run has no such field).
    pub expected: Option<u64>,
    /// Its bits in the actual run (`None` if that run has no such field).
    pub actual: Option<u64>,
}

/// Where two runs part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    /// The first tick whose hash differs.
    pub tick: u32,
    /// The expected run's hash at that tick (`None` past its end).
    pub expected_hash: Option<u64>,
    /// The actual run's hash at that tick (`None` past its end).
    pub actual_hash: Option<u64>,
    /// The fields that differ at that tick, in hash order.
    pub fields: Vec<FieldDiff>,
}

impl fmt::Display for Divergence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "first diverging tick {}", self.tick)?;
        if let (Some(e), Some(a)) = (self.expected_hash, self.actual_hash) {
            write!(f, " (expected {e:016x}, got {a:016x})")?;
        }
        for d in &self.fields {
            write!(f, "; {}: ", d.name)?;
            match d.expected {
                Some(e) => write!(f, "{e:#x}")?,
                None => write!(f, "absent")?,
            }
            match d.actual {
                Some(a) => write!(f, " -> {a:#x}")?,
                None => write!(f, " -> absent")?,
            }
        }
        Ok(())
    }
}

/// The fields of two logs that differ, matched by position and name.
pub fn diff_fields(expected: &[FieldValue], actual: &[FieldValue]) -> Vec<FieldDiff> {
    let mut out = Vec::new();
    for i in 0..expected.len().max(actual.len()) {
        match (expected.get(i), actual.get(i)) {
            (Some(e), Some(a)) if e == a => {}
            (Some(e), Some(a)) if e.name == a.name => out.push(FieldDiff {
                name: e.name.clone(),
                expected: Some(e.bits),
                actual: Some(a.bits),
            }),
            (e, a) => {
                if let Some(e) = e {
                    out.push(FieldDiff {
                        name: e.name.clone(),
                        expected: Some(e.bits),
                        actual: None,
                    });
                }
                if let Some(a) = a {
                    out.push(FieldDiff {
                        name: a.name.clone(),
                        expected: None,
                        actual: Some(a.bits),
                    });
                }
            }
        }
    }
    out
}

/// Record `expected` and `actual` for `run` (`injection` applied to `actual` only) and report the
/// first diverging tick with the fields that differ there; `Ok(None)` if every tick agrees.
pub fn compare(
    expected: &mut dyn DetScenario,
    actual: &mut dyn DetScenario,
    run: &DetRun,
    injection: Option<Injection>,
) -> Result<Option<Divergence>, DetError> {
    let a = record(expected, run);
    let b = record_injected(actual, run, injection)?;
    let Some(tick) = first_divergence(&a.tick_hashes, &b.tick_hashes) else {
        return Ok(None);
    };
    let fields = if tick < run.ticks {
        let e = fields_at(expected, run, tick, None)?;
        let f = fields_at(actual, run, tick, injection)?;
        diff_fields(&e, &f)
    } else {
        Vec::new()
    };
    Ok(Some(Divergence {
        tick,
        expected_hash: a.tick_hashes.get(tick as usize).copied(),
        actual_hash: b.tick_hashes.get(tick as usize).copied(),
        fields,
    }))
}

/// `<arch>-<os>` of the running build (`x86_64-linux`, `aarch64-macos`, `aarch64-android`,
/// `x86_64-windows`). The Steam Deck is `x86_64-linux`.
pub fn current_target() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

/// The golden file of `scenario` in `dir`.
pub fn golden_path(dir: &Path, scenario: &str) -> PathBuf {
    dir.join(format!("{scenario}.{EXTENSION}"))
}

/// Where the engine scenarios' goldens live: `$REGIA_DET_GOLDEN_DIR` if set, else this crate's
/// `goldens/` directory in the source tree.
#[expect(
    clippy::disallowed_methods,
    reason = "harness setting: where goldens live; no sim tick reads the environment (ADR-8367)"
)]
pub fn engine_golden_dir() -> PathBuf {
    // det-scan: allow(env_read): the harness picks where goldens live; no sim tick reads it
    match std::env::var_os(GOLDEN_DIR_ENV) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => Path::new(env!("CARGO_MANIFEST_DIR")).join("goldens"),
    }
}

/// A committed golden: the run, a run hash per target, and the per-tick hashes of one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Golden {
    /// The scenario's name.
    pub scenario: String,
    /// The run the hashes were recorded with.
    pub run: DetRun,
    /// `(target, run hash)` for every target that has been recorded, sorted by target.
    pub target_hashes: Vec<(String, u64)>,
    /// The target whose per-tick hashes are kept (`None` if none are).
    pub trace_target: Option<String>,
    /// That target's per-tick hashes (empty if `trace_target` is `None`).
    pub tick_hashes: Vec<u64>,
}

impl Golden {
    /// A golden that records `trace` as `target`'s, with its per-tick hashes.
    pub fn from_trace(trace: &DetTrace, target: &str) -> Golden {
        Golden {
            scenario: trace.scenario.clone(),
            run: trace.run,
            target_hashes: vec![(target.to_string(), trace.run_hash())],
            trace_target: Some(target.to_string()),
            tick_hashes: trace.tick_hashes.clone(),
        }
    }

    /// The run hash recorded for `target`.
    pub fn hash_for(&self, target: &str) -> Option<u64> {
        self.target_hashes
            .iter()
            .find(|(t, _)| t == target)
            .map(|&(_, h)| h)
    }

    /// The reference run hash: the trace target's, or the first recorded one.
    pub fn reference(&self) -> Option<(&str, u64)> {
        self.trace_target
            .as_deref()
            .and_then(|t| self.hash_for(t).map(|h| (t, h)))
            .or_else(|| self.target_hashes.first().map(|(t, h)| (t.as_str(), *h)))
    }

    /// Record (or replace) `target`'s run hash.
    pub fn set_hash(&mut self, target: &str, hash: u64) {
        match self.target_hashes.iter_mut().find(|(t, _)| t == target) {
            Some(entry) => entry.1 = hash,
            None => {
                self.target_hashes.push((target.to_string(), hash));
                self.target_hashes.sort();
            }
        }
    }

    /// The golden as text (the `.detgold` format; see the book page "Determinism goldens").
    pub fn to_text(&self) -> String {
        use fmt::Write as _;
        let mut s = String::new();
        let _ = writeln!(
            s,
            "# Determinism golden (ADR-8046). Written with {BLESS_ENV}=1; do not edit by hand."
        );
        let _ = writeln!(s, "format={FORMAT}");
        let _ = writeln!(s, "scenario={}", self.scenario);
        let _ = writeln!(s, "seed={:#018x}", self.run.seed);
        let _ = writeln!(s, "ticks={}", self.run.ticks);
        let _ = writeln!(s, "rate_hz={}", self.run.rate.hz());
        for (target, hash) in &self.target_hashes {
            let _ = writeln!(s, "hash.{target}={hash:016x}");
        }
        if let Some(target) = &self.trace_target {
            let _ = writeln!(s, "trace_target={target}");
            for h in &self.tick_hashes {
                let _ = writeln!(s, "t={h:016x}");
            }
        }
        s
    }

    /// Parse the `.detgold` text. Every key is required once except `hash.*` (one per target),
    /// `trace_target` and `t` (one per tick, only with `trace_target`).
    pub fn parse(text: &str) -> Result<Golden, DetError> {
        let bad = |why: String| DetError::GoldenInvalid(why);
        let mut format = None;
        let mut scenario = None;
        let mut seed = None;
        let mut ticks = None;
        let mut rate = None;
        let mut target_hashes: Vec<(String, u64)> = Vec::new();
        let mut trace_target = None;
        let mut tick_hashes = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let n = i + 1;
            let Some((key, value)) = line.split_once('=') else {
                return Err(bad(format!("line {n}: expected key=value")));
            };
            let once = |slot: &Option<String>| -> Result<(), DetError> {
                if slot.is_some() {
                    Err(DetError::GoldenInvalid(format!("line {n}: `{key}` twice")))
                } else {
                    Ok(())
                }
            };
            match key {
                "format" => {
                    once(&format)?;
                    format = Some(value.to_string());
                }
                "scenario" => {
                    once(&scenario)?;
                    validate_name(value).map_err(|e| bad(format!("line {n}: {e}")))?;
                    scenario = Some(value.to_string());
                }
                "seed" => {
                    once(&seed)?;
                    seed = Some(value.to_string());
                }
                "ticks" => {
                    once(&ticks)?;
                    ticks = Some(value.to_string());
                }
                "rate_hz" => {
                    once(&rate)?;
                    rate = Some(value.to_string());
                }
                "trace_target" => {
                    once(&trace_target)?;
                    validate_target(value).map_err(|why| bad(format!("line {n}: {why}")))?;
                    trace_target = Some(value.to_string());
                }
                "t" => {
                    if trace_target.is_none() {
                        return Err(bad(format!("line {n}: `t` before `trace_target`")));
                    }
                    tick_hashes.push(parse_hex(value).map_err(|why| bad(format!("line {n}: {why}")))?);
                }
                _ => {
                    let Some(target) = key.strip_prefix("hash.") else {
                        return Err(bad(format!("line {n}: unknown key `{key}`")));
                    };
                    validate_target(target).map_err(|why| bad(format!("line {n}: {why}")))?;
                    if target_hashes.iter().any(|(t, _)| t == target) {
                        return Err(bad(format!("line {n}: `{key}` twice")));
                    }
                    let hash = parse_hex(value).map_err(|why| bad(format!("line {n}: {why}")))?;
                    target_hashes.push((target.to_string(), hash));
                }
            }
        }
        match format.as_deref() {
            Some(FORMAT) => {}
            Some(other) => return Err(bad(format!("format `{other}` is not `{FORMAT}`"))),
            None => return Err(bad("no `format` line".into())),
        }
        let scenario = scenario.ok_or_else(|| bad("no `scenario` line".into()))?;
        let seed = seed.ok_or_else(|| bad("no `seed` line".into()))?;
        let seed = parse_hex(seed.strip_prefix("0x").unwrap_or(&seed))?;
        let ticks: u32 = ticks
            .ok_or_else(|| bad("no `ticks` line".into()))?
            .parse()
            .map_err(|_| bad("`ticks` is not a u32".into()))?;
        let rate = rate
            .ok_or_else(|| bad("no `rate_hz` line".into()))?
            .parse::<u32>()
            .ok()
            .and_then(TickRate::new)
            .ok_or_else(|| bad("`rate_hz` is not a positive u32".into()))?;
        if trace_target.is_some() && tick_hashes.len() != ticks as usize {
            return Err(bad(format!("{} `t` lines for {ticks} ticks", tick_hashes.len())));
        }
        if let Some(t) = &trace_target
            && !target_hashes.iter().any(|(h, _)| h == t)
        {
            return Err(bad(format!("trace_target `{t}` has no `hash.{t}` line")));
        }
        target_hashes.sort();
        Ok(Golden {
            scenario,
            run: DetRun::new(seed, ticks, rate),
            target_hashes,
            trace_target,
            tick_hashes,
        })
    }
}

fn validate_target(target: &str) -> Result<(), String> {
    let ok = !target.is_empty()
        && target.len() <= MAX_NAME_LEN
        && target
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "target `{target}` must be <arch>-<os> in a-z, 0-9, _ and -"
        ))
    }
}

fn parse_hex(value: &str) -> Result<u64, DetError> {
    if value.is_empty() || value.len() > 16 {
        return Err(DetError::GoldenInvalid(format!(
            "`{value}` is not 1-16 hex digits"
        )));
    }
    u64::from_str_radix(value, 16).map_err(|_| DetError::GoldenInvalid(format!("`{value}` is not hex")))
}

/// Whether a golden check compares or writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoldenMode {
    /// Compare with the committed golden.
    Check,
    /// Write this target's hash (and the per-tick trace when the file is new, the run changed, or
    /// this target is the trace target).
    Bless,
}

impl GoldenMode {
    /// [`GoldenMode::Bless`] when `REGIA_BLESS=1`, else [`GoldenMode::Check`].
    #[expect(
        clippy::disallowed_methods,
        reason = "harness mode: bless or check; the scenario steps the same either way (ADR-8367)"
    )]
    pub fn from_env() -> Self {
        // det-scan: allow(env_read): bless or check is a harness mode; the scenario steps the same either way
        if std::env::var(BLESS_ENV).as_deref() == Ok("1") {
            GoldenMode::Bless
        } else {
            GoldenMode::Check
        }
    }
}

/// The outcome of a golden check. [`DetStatus::code`] is the protocol code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetStatus {
    /// The run hash equals this target's committed hash and, unless planted, the reference.
    Ok,
    /// This target has no line of its own and the run equals the reference target's hash.
    UnrecordedTarget,
    /// The golden was written (bless mode).
    Blessed,
    /// The run hash differs from the committed one.
    Diverged,
    /// A clean run matches its target hash but differs from the reference target.
    DisagreesWithReference,
    /// Blessing a clean non-reference target would record a cross-target disagreement.
    BlessRefusedCleanDiffers,
    /// No golden file.
    GoldenMissing,
    /// The golden was recorded with another seed, tick count, rate or scenario name.
    GoldenStale,
    /// The golden does not parse or cannot be read.
    GoldenInvalid,
    /// Bless mode could not write the golden.
    GoldenUnwritable,
}

impl DetStatus {
    /// The stable, lowercase code.
    pub const fn code(self) -> &'static str {
        match self {
            DetStatus::Ok => "ok",
            DetStatus::UnrecordedTarget => "unrecorded_target",
            DetStatus::Blessed => "blessed",
            DetStatus::Diverged => "diverged",
            DetStatus::DisagreesWithReference => "disagrees_with_reference",
            DetStatus::BlessRefusedCleanDiffers => "bless_refused_clean_differs",
            DetStatus::GoldenMissing => "golden_missing",
            DetStatus::GoldenStale => "golden_stale",
            DetStatus::GoldenInvalid => "golden_invalid",
            DetStatus::GoldenUnwritable => "golden_unwritable",
        }
    }

    /// `ok`, `unrecorded_target` and `blessed` pass; the rest fail.
    pub const fn passed(self) -> bool {
        matches!(
            self,
            DetStatus::Ok | DetStatus::UnrecordedTarget | DetStatus::Blessed
        )
    }
}

/// The report of one golden check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetReport {
    /// The scenario's name.
    pub scenario: String,
    /// The target this build runs on ([`current_target`]).
    pub target: String,
    /// The run that was checked.
    pub run: DetRun,
    /// This build's run hash.
    pub hash: u64,
    /// The hash it was compared with (this target's, else the reference's).
    /// Reference-policy failures report the reference hash.
    pub expected: Option<u64>,
    /// The outcome.
    pub status: DetStatus,
    /// The first tick whose hash differs from the golden's per-tick trace.
    pub first_diverging_tick: Option<u32>,
    /// The golden's hash at that tick.
    pub expected_tick_hash: Option<u64>,
    /// This build's hash at that tick.
    pub actual_tick_hash: Option<u64>,
    /// Whether this build's run hash equals the reference target's (`None` without a reference).
    pub agrees_with_reference: Option<bool>,
    /// The golden file.
    pub golden: PathBuf,
    /// A sentence for a human.
    pub detail: String,
}

impl DetReport {
    /// Whether the check passed ([`DetStatus::passed`]).
    pub const fn passed(&self) -> bool {
        self.status.passed()
    }

    /// The report as one line of JSON (the fields of [`DetReport`], hashes as 16 hex digits,
    /// absent values as `null`). Runners print it after [`REPORT_PREFIX`].
    pub fn to_json(&self) -> String {
        let hex = |h: Option<u64>| h.map_or_else(|| "null".to_string(), |h| format!("\"{h:016x}\""));
        let num = |n: Option<u32>| n.map_or_else(|| "null".to_string(), |n| n.to_string());
        let agrees = self
            .agrees_with_reference
            .map_or_else(|| "null".to_string(), |b| b.to_string());
        format!(
            "{{\"scenario\":{},\"target\":{},\"seed\":\"{:#018x}\",\"ticks\":{},\"rate_hz\":{},\"status\":\"{}\",\
             \"hash\":\"{:016x}\",\"expected\":{},\"first_diverging_tick\":{},\"expected_tick_hash\":{},\
             \"actual_tick_hash\":{},\"agrees_with_reference\":{},\"golden\":{},\"detail\":{}}}",
            json_string(&self.scenario),
            json_string(&self.target),
            self.run.seed,
            self.run.ticks,
            self.run.rate.hz(),
            self.status.code(),
            self.hash,
            hex(self.expected),
            num(self.first_diverging_tick),
            hex(self.expected_tick_hash),
            hex(self.actual_tick_hash),
            agrees,
            json_string(&self.golden.display().to_string()),
            json_string(&self.detail),
        )
    }
}

impl fmt::Display for DetReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} on {}: {} ({})",
            self.scenario,
            self.target,
            self.status.code(),
            self.detail
        )
    }
}

/// A JSON string literal for `s`.
fn json_string(s: &str) -> String {
    use fmt::Write as _;
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Record `scenario`'s default run and check it against its golden in `dir` (see [`check_trace`]).
///
/// The run starts on the IEEE default floating-point environment ([`record`] puts it back); when the
/// calling thread was not on it (flush-to-zero, another rounding mode, traps), the report's detail
/// says so, because the same thread's other float code ran under that environment.
pub fn check_golden(scenario: &mut dyn DetScenario, dir: &Path, mode: GoldenMode) -> DetReport {
    let env = crate::fpu::ensure_default_fp_env();
    let run = scenario.default_run();
    let trace = record(scenario, &run);
    let mut report = check_trace(
        &trace,
        &golden_path(dir, &trace.scenario),
        &current_target(),
        mode,
    );
    if !env.was_default() {
        report.detail.push_str(&format!(
            "; the thread's fp environment was not the default before the run (control {:#x}) and was reset",
            env.raw_before
        ));
    }
    report
}

/// Check `trace`, recorded on `target`, against the golden at `path` (or write it in bless mode).
/// Clean traces must agree with the reference even if their own target hash matches.
/// Blessing a differing non-reference target is refused unless [`DetTrace::planted`] is true.
pub fn check_trace(trace: &DetTrace, path: &Path, target: &str, mode: GoldenMode) -> DetReport {
    let mut report = DetReport {
        scenario: trace.scenario.clone(),
        target: target.to_string(),
        run: trace.run,
        hash: trace.run_hash(),
        expected: None,
        status: DetStatus::GoldenMissing,
        first_diverging_tick: None,
        expected_tick_hash: None,
        actual_tick_hash: None,
        agrees_with_reference: None,
        golden: path.to_path_buf(),
        detail: String::new(),
    };
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => Some(Golden::parse(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            report.status = DetStatus::GoldenInvalid;
            report.detail = format!("cannot read {}: {e}", path.display());
            return report;
        }
    };
    if mode == GoldenMode::Bless {
        return bless(trace, path, target, existing, report);
    }
    let golden = match existing {
        None => {
            report.detail = format!(
                "no golden at {}; record it with {BLESS_ENV}=1 on {REFERENCE_TARGET}",
                path.display()
            );
            return report;
        }
        Some(Err(e)) => {
            report.status = DetStatus::GoldenInvalid;
            report.detail = e.to_string();
            return report;
        }
        Some(Ok(golden)) => golden,
    };
    if golden.scenario != trace.scenario || golden.run != trace.run {
        report.status = DetStatus::GoldenStale;
        report.detail = format!(
            "golden records {} seed {:#x}, {} ticks at {} Hz; this run is {} seed {:#x}, {} ticks at {} Hz",
            golden.scenario,
            golden.run.seed,
            golden.run.ticks,
            golden.run.rate.hz(),
            trace.scenario,
            trace.run.seed,
            trace.run.ticks,
            trace.run.rate.hz()
        );
        return report;
    }
    let reference = golden.reference().map(|(t, h)| (t.to_string(), h));
    report.agrees_with_reference = reference.as_ref().map(|(_, h)| *h == report.hash);
    let own = golden.hash_for(target);
    report.expected = own.or(reference.as_ref().map(|(_, h)| *h));
    report.status = match (own, report.expected) {
        (Some(e), _) if e == report.hash => DetStatus::Ok,
        (None, Some(e)) if e == report.hash => DetStatus::UnrecordedTarget,
        (_, None) => DetStatus::GoldenInvalid,
        _ => DetStatus::Diverged,
    };
    if report.status == DetStatus::Ok && !trace.planted && report.agrees_with_reference == Some(false) {
        report.status = DetStatus::DisagreesWithReference;
        report.expected = reference.as_ref().map(|(_, h)| *h);
    }
    match report.status {
        DetStatus::Ok => {
            report.detail = format!("{} ticks match this target's committed hash", trace.run.ticks);
        }
        DetStatus::UnrecordedTarget => {
            let reference = reference.map(|(t, _)| t).unwrap_or_default();
            report.detail = format!(
                "no hash.{target} line yet; equals the reference {reference} (record it with {BLESS_ENV}=1)"
            );
        }
        DetStatus::GoldenInvalid => report.detail = "golden has no hash line".into(),
        _ => describe_divergence(trace, &golden, &mut report),
    }
    report
}

fn describe_divergence(trace: &DetTrace, golden: &Golden, report: &mut DetReport) {
    if !golden.tick_hashes.is_empty()
        && let Some(tick) = first_divergence(&golden.tick_hashes, &trace.tick_hashes)
    {
        report.first_diverging_tick = Some(tick);
        report.expected_tick_hash = golden.tick_hashes.get(tick as usize).copied();
        report.actual_tick_hash = trace.tick_hashes.get(tick as usize).copied();
        let trace_target = golden.trace_target.as_deref().unwrap_or("?");
        report.detail = format!(
            "first diverging tick {tick} of {} against the {trace_target} trace",
            trace.run.ticks
        );
    } else if golden.tick_hashes.is_empty() {
        report.detail = "run hash differs; the golden keeps no per-tick trace".into();
    } else {
        report.detail = format!(
            "every tick equals the {} trace, but this target's committed hash differs",
            golden.trace_target.as_deref().unwrap_or("?")
        );
    }
}

fn bless(
    trace: &DetTrace,
    path: &Path,
    target: &str,
    existing: Option<Result<Golden, DetError>>,
    mut report: DetReport,
) -> DetReport {
    let golden = match existing {
        Some(Ok(mut g)) if g.scenario == trace.scenario && g.run == trace.run => {
            if !trace.planted
                && let Some((reference_target, reference_hash)) = g.reference()
                && target != reference_target
                && report.hash != reference_hash
            {
                report.status = DetStatus::BlessRefusedCleanDiffers;
                report.expected = Some(reference_hash);
                report.agrees_with_reference = Some(false);
                describe_divergence(trace, &g, &mut report);
                return report;
            }
            g.set_hash(target, trace.run_hash());
            if g.trace_target.is_none() || g.trace_target.as_deref() == Some(target) {
                g.trace_target = Some(target.to_string());
                g.tick_hashes = trace.tick_hashes.clone();
            }
            g
        }
        _ => Golden::from_trace(trace, target),
    };
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("{EXTENSION}.tmp{}", std::process::id()));
        std::fs::write(&tmp, golden.to_text())?;
        std::fs::rename(&tmp, path)
    };
    report.expected = Some(report.hash);
    report.agrees_with_reference = golden.reference().map(|(_, hash)| hash == report.hash);
    match write() {
        Ok(()) => {
            report.status = DetStatus::Blessed;
            report.detail = format!("wrote hash.{target} to {}", path.display());
        }
        Err(e) => {
            report.status = DetStatus::GoldenUnwritable;
            report.detail = format!("cannot write {}: {e}", path.display());
        }
    }
    report
}

/// Makes a fresh scenario instance.
pub type ScenarioFactory = Box<dyn Fn() -> Box<dyn DetScenario> + Send + Sync>;

/// One registered scenario.
pub struct DetEntry {
    name: String,
    golden_dir: PathBuf,
    factory: ScenarioFactory,
}

impl DetEntry {
    /// The scenario's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The directory its golden lives in.
    pub fn golden_dir(&self) -> &Path {
        &self.golden_dir
    }

    /// A fresh instance.
    pub fn create(&self) -> Box<dyn DetScenario> {
        (self.factory)()
    }
}

impl fmt::Debug for DetEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DetEntry")
            .field("name", &self.name)
            .field("golden_dir", &self.golden_dir)
            .finish_non_exhaustive()
    }
}

/// The scenarios one test binary checks: the engine's and any a game registers.
#[derive(Debug, Default)]
pub struct DetRegistry {
    entries: Vec<DetEntry>,
}

impl DetRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        DetRegistry::default()
    }

    /// A registry with the engine scenarios ([`scenarios::engine_scenarios`]) and their goldens in
    /// [`engine_golden_dir`].
    pub fn with_engine_scenarios() -> Self {
        let mut registry = DetRegistry::new();
        let dir = engine_golden_dir();
        for factory in scenarios::engine_scenarios() {
            // The engine names are fixed, valid and distinct (`engine_scenarios_have_valid_names`).
            let _ = registry.register(dir.clone(), factory);
        }
        registry
    }

    /// Register a scenario whose golden lives in `golden_dir`. The factory is called once to read
    /// the name, which must be valid and not yet registered.
    pub fn register(
        &mut self,
        golden_dir: impl Into<PathBuf>,
        factory: impl Fn() -> Box<dyn DetScenario> + Send + Sync + 'static,
    ) -> Result<(), DetError> {
        let name = factory().name().to_string();
        validate_name(&name)?;
        if self.entries.iter().any(|e| e.name == name) {
            return Err(DetError::DuplicateScenario(name));
        }
        self.entries.push(DetEntry {
            name,
            golden_dir: golden_dir.into(),
            factory: Box::new(factory),
        });
        Ok(())
    }

    /// The registered entries, in registration order.
    pub fn entries(&self) -> &[DetEntry] {
        &self.entries
    }

    /// The entry called `name`.
    pub fn get(&self, name: &str) -> Option<&DetEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Check every registered scenario against its golden.
    pub fn check_all(&self, mode: GoldenMode) -> Vec<DetReport> {
        self.entries
            .iter()
            .map(|e| check_golden(e.create().as_mut(), &e.golden_dir, mode))
            .collect()
    }
}

/// Zero-order hold: for each tick time `k * dt`, the last sample whose time `t_ms(sample) / 1000`
/// is at or before it. The trace covers ticks `0..=floor(last_t / dt)`; an empty trace gives none.
pub fn resample_zoh<S>(samples: &[S], dt: f32, t_ms: impl Fn(&S) -> u64) -> Vec<&S> {
    let Some(last) = samples.last() else {
        return Vec::new();
    };
    let last_t = t_ms(last) as f32 / 1000.0;
    let n_ticks = ((last_t / dt).floor() as usize) + 1;
    let mut out = Vec::with_capacity(n_ticks);
    let mut i = 0;
    for k in 0..n_ticks {
        let t = k as f32 * dt;
        while i + 1 < samples.len() && (t_ms(&samples[i + 1]) as f32 / 1000.0) <= t {
            i += 1;
        }
        out.push(&samples[i]);
    }
    out
}
