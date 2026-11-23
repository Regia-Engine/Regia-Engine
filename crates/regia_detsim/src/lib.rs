// SPDX-License-Identifier: MPL-2.0
//! Lockstep simulation kit (ADR-8012 A-4 and A-5, ADR-8013).
//!
//! The pieces a deterministic, lockstep game simulation needs so that every peer, on every
//! supported OS and CPU architecture, computes the same bits from the same inputs:
//!
//! - [`math`]: transcendentals through the pure-Rust `libm` 0.2.16 (pinned exactly), never the
//!   platform's libm or the standard library's float methods.
//! - [`tick`]: the authoritative tick rate (`60 Hz` by default) and the seconds-to-ticks
//!   conversions, with [`tick::TickRate`] for a host that runs another rate.
//! - [`fpu`]: the FTZ/DAZ guard that puts a sim thread's floating-point environment back to the
//!   IEEE default at sim-thread start.
//! - [`float`]: `det_min`, `det_max`, `det_clamp` and `canonical`: float `min`/`max`/`clamp` with one
//!   zero sign and one NaN pattern on every host and optimization level (std's do not have that).
//! - [`fog`]: fog of war, team visibility as a pure function of sim state, with a data sight table
//!   and the [`los`] line-of-sight hook (plan row S-16, ADR-8154).
//! - [`hash`]: the per-system desync hash (a hashing `serde::Serializer` with a canonical float
//!   rule, the 240-tick ring, the divergence report and injection; spec 120's `DesyncHash`).
//! - [`golden`]: the deterministic golden harness (ADR-8046): a [`golden::DetScenario`] stepped
//!   for N ticks, a hash per tick, a committed hash per target, the first diverging tick, and the
//!   60 Hz vs 120 Hz rate gates.
//! - [`ids`]: [`ids::DetIdAllocator`], entity ids with FIFO reuse and generations (ADR-8048).
//! - [`broadphase`]: [`broadphase::DetBroadphase`], a grid broadphase that visits the same contacts
//!   in the same order as the all-pairs loop, with height bands (ADR-8048).
//! - [`contact`]: mass-weighted push-apart, restitution and friction impulses and ram reports for
//!   the broadphase's contacts, in the same order ([`contact::resolve_all`], ADR-8563).
//! - [`ai`]: the deterministic AI core: the time-sliced scheduler, the seeded `AiRng` and the utility scorer
//!   with the product rule (plan row S-09, ADR-8152).
//! - [`harness`], [`soak`] and [`profile`]: bounded seeded runs, paired checks at
//!   every tick, and a per-system tick profiler ([`profile::TickProfiler`]) keyed by the
//!   same [`hash::Systems`] list as the desync hash. The profiler is a separate struct:
//!   it never writes sim state. Its clock is per-thread CPU time, or monotonic wall time
//!   when the operating system has no thread clock (ADR-8498).
//! - [`arena`]: a configurable headless AI-vs-AI sample implementing F-02.
//! - [`scale_soak`]: the configurable 40-seat, 900-entity load scenario of plan row F-07 and its
//!   pass timing (ADR-8148; named `soak` on its lane, renamed at integ-v06n beside S-18's [`soak`]).
//! - [`hover`]: the lockstep hover step (ADR-8079), a pure `step(params, state, input, floor, dt)`,
//!   with [`ballistic`] (one ballistic step for jumps, falls and arcs), [`site_physics`] (a world's
//!   gravity as a weight scale) and [`lowg`] (the low-g jump and mortar rules).
//! - [`projectile`]: deterministic projectiles (ADR-8509): gravity, drag, wind, swept hits through
//!   the [`projectile::SegmentHits`] seam, bounces, fuse, lifetime, range and a fixed-capacity pool
//!   with impacts in slot order; [`aim`]: launch angles without and with drag.
//! - [`state`]: saving, resuming and sending a lockstep state ([`state::DetState`], a raw-bits
//!   byte codec, framed snapshots and the [`state::SnapshotRing`] of checkpoints).
//! - [`replay`]: per-tick input logs, offline replay against recorded hashes, bounded catch-up and
//!   the [`replay::DesyncReplay`] hook (DC-3.7b).
//! - [`rng`]: [`rng::DetRng`], a seeded xoshiro256** generator with independent per-system streams,
//!   exactly unbiased ranges and exact unit floats.
//! - [`fixed`]: integer millimeters ([`fixed::Mm`], [`fixed::Mm2`]) with explicit overflow, an
//!   integer-only f32 rounding, and exact 2D integer dot, cross and square root.
//!
//! The crate depends on no ECS or GPU crate, so a game's sim and a headless server link it alone.
//! The tick profiler reads the thread CPU clock through `libc` or `windows-sys` and never writes
//! that reading into hashed state. Its public API is semver-versioned; a change that moves any
//! output bit is a sim-version change (ADR-8012 A-4).
//!
//! The rules for code that runs on a lockstep tick (no std transcendentals, no std float
//! `min`/`max`/`clamp`, no glam constructors or methods that call platform trig, no hash-container
//! iteration, no rayon, no SIMD vector types, no f64 parse narrowed to f32) are checked by
//! `cargo xtask det-scan` over the crates on `docs/ci/lockstep-closure.toml`, and the lockstep
//! crates carry a `clippy.toml` that denies the std float methods and those glam methods. See the
//! book page "Lockstep math and the determinism scan".
//!
//! [`handling`] flies a hull over scripted ground and reports the feel targets (ADR-8393).
//!
//! ```
//! use regia_detsim::{fpu, math, tick};
//!
//! // At sim-thread start.
//! let report = fpu::ensure_default_fp_env();
//! assert!(report.was_default() || report.supported);
//!
//! let dt = tick::tick_dt();
//! let heading = math::atan2(1.0, 1.0);
//! let step = [math::cos(heading) * dt, math::sin(heading) * dt];
//! assert!(step[0] > 0.0 && step[1] > 0.0);
//! ```

// Plan row S-09 (ADR-8152).
pub mod ai;
pub mod broadphase;
pub mod fixed;
pub mod float;
pub mod fpu;
pub mod golden;
pub mod hash;
pub mod ids;
pub mod math;
pub mod rng;
pub mod tick;
// Plan row S-16 (ADR-8154).
pub mod fog;
pub mod los;

/// Integer reciprocal local avoidance with default-preserving host settings.
pub mod avoid;

pub mod arena;
pub mod harness;
pub mod profile;
pub mod replay;
pub mod scale_soak;
pub mod soak;

pub mod spatial;

pub mod ballistic;
pub mod ground_vehicle;
pub mod handling;
pub mod hover;
pub mod lowg;
pub mod site_physics;

mod platform;

pub mod aim;
pub mod projectile;
pub mod state;

// Contact response for the broadphase bodies (ADR-8563).
pub mod contact;

// Render-free debug draw of the deterministic indexes, capture off by default (ADR-8562).
pub mod debug;

/// The version of the bits [`math`] produces (ADR-8876). Lockstep peers compare it with the content hash
/// (spec 121 § 14, ADR-8084): every change to the bits of a `math` function bumps it, and every det golden
/// or pin that moves with it names the bump.
///
/// - `0`: the regia-absorb copy, every function the bare libm 0.2.16 call (the source game's sim bits,
///   except its own canonical zero and NaN steps around `atan2f`).
/// - `1`: [`math::atan2`] is the correctly rounded `atan2` with signed-zero inputs as `+0`, NaN results
///   canonical and no `-0` result; the old bits are [`math::atan2_libm`]. Every other function is
///   unchanged.
pub const MATH_BITS_VERSION: u32 = 1;
