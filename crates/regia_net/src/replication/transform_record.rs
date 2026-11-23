// SPDX-License-Identifier: MPL-2.0
//! The quantized transform record (spec 120 § 8.2, ADR-8467).
//!
//! A transform travels as integers: the position as whole steps of the entity profile's
//! `position_step_m` (a root; a replicated child or a profile without a step sends raw `f32`), the
//! rotation as smallest-three at 10 bits per kept component, and the velocity at 3 x 12 bits of
//! 1/32 m/s (raw `f32` past +-64 m/s). G. Fiedler, "Snapshot Compression" (gafferongames.com, 2015)
//! and the public networked-physics series ("Quantization"); the 20-bit cell offset and the field
//! widths follow the DO27 state-sync design as spec 120 § 8.2 describes it (re-derived, no code).
//!
//! The absolute form is what a spawn, a tick-stream update and an update without a usable baseline
//! carry. On the acknowledged state channel an update is a delta against the baseline the client
//! acknowledged (the version `base_ago` names): one change bit per part and per component, and a
//! short residual for what changed. Both sides hold the baseline's integers, so the client
//! reconstructs exactly the server's quantized value: the delta adds no error.
//!
//! ```text
//! abs      := pos | [rot | vel]  (Movement, Full) | [scale] (Full)
//! pos      := 1 | default_step u1 | [step f32] | (cell zigzag varint | offset u20) x3
//!           | 0 | f32 x3
//! rot      := largest u2 | kept u10 x3
//! vel      := present u1 | [1 | i12 x3 (1/32 m/s) | 0 | f32 x3]
//! scale    := present u1 | [f32 x3]
//!
//! delta    := pos_d | [rot_d | vel_d] (Movement, Full) | [scale_d] (Full); the baseline's mode
//! pos_d    := changed u1 | [1 | [predicted u1] | (changed u1 | [residual]) x3   (both on one grid)
//!                         | 0 | pos]
//!             predicted is present when the baseline has a grid velocity and the record is newer
//!             than it: 1 codes the residual against the baseline moved along that velocity
//!             (`predicted_steps`, ADR-8522), 0 against the baseline itself
//! rot_d    := changed u1 | [1 | (changed u1 | [residual]) x3     (the same largest component)
//!                         | 0 | rot]
//! vel_d    := changed u1 | [1 | (changed u1 | [residual]) x3     (both on the velocity grid)
//!                         | 0 | vel]
//! scale_d  := changed u1 | [scale]
//! residual := class u2 | 0: u4 | 1: u8 | 2: u14 | 3: varint   of zigzag(d) - 1
//!
//! far      := 1 | x u15 | z u15 | y u10                    (the 5-byte record of spec 120 § 8.3)
//!           | 0 | (changed u1 | [residual]) x3  (x, z, y)  (against the baseline's coarse position)
//! ```
//!
//! **The far record (revision 4, ADR-10052 (1) as amended by ADR-10124).** A far-tier update of a
//! grid position carries only a coarse position: the offset inside the entity's cell rounded to
//! `2^5` steps on x and z and `2^10` steps on y (3.2 cm and 1.02 m at the default 1 mm step, so
//! the shown position is within 1.6 cm and 0.51 m of the server's). The cell is the baseline's:
//! the server sends the form only while the baseline it names is in the same cell on every axis.
//! The coarse position is a function of the server's position alone, whatever the baseline, so
//! two far records of one version always decode to the same position, and the residual form
//! (against the baseline's coarse position, which both sides compute from either form of the
//! baseline) is exact however records were lost or reordered. Rotation and scale stay as the
//! baseline has them and the velocity is dropped (the client infers it from the positions,
//! spec 120 § 9.7). A far position is never a baseline for a transform delta: the exact record
//! that follows it is absolute.

use glam::{Quat, Vec3};

use super::{NetTransform, TransformReplication};
use crate::bits::{BitReader, BitWriter, DecodeError};
use crate::codec::Rotation;
use crate::profile::PositionQuantizer;

#[cfg(test)]
mod far_review;

/// Revision of the transform record, mixed into the protocol hash (`runtime::protocol_hash`) so
/// builds whose transform records differ refuse each other at the handshake (`protocol_mismatch`)
/// instead of misreading records. 1 is the raw `f32` record before ADR-8467 (a build with
/// revision 1 has the protocol hashes of the builds before it); 2 is the quantized record with
/// deltas against the acknowledged baseline; 3 adds the steady bit and the velocity-predicted
/// position residual (ADR-8522); 4 adds the far-tier record (state record kind 3, ADR-10124).
pub const TRANSFORM_RECORD_REVISION: u64 = 4;

/// The row of `runtime::protocol_types` that names the transform record: a dev client of another
/// revision sees `regia_net::replication::transform_record (different)` (or `missing on client`
/// against a build from before ADR-8467) in its `protocol_mismatch`.
pub const TRANSFORM_RECORD_TYPE: &str = "regia_net::replication::transform_record";

/// Bits of a position's offset inside its cell: a cell is `2^20` steps (1,048.576 m at the
/// default 1 mm, 1,024 m at 1/1024 m).
pub const POSITION_CELL_BITS: u32 = 20;

/// The velocity grid in metres per second (12-bit components reach +-63.97 m/s; faster velocities
/// travel as raw `f32`).
pub const VELOCITY_STEP_MPS: f32 = 1.0 / 32.0;

/// The position step a record names with one bit (the default profile's `position_step_m`).
pub const DEFAULT_WIRE_STEP_M: f32 = 0.001;

/// The tick rate the position predictor assumes (ADR-8522): a predicted residual is coded against
/// the baseline moved by its velocity for `ticks / PREDICTION_TICK_HZ` seconds. It is part of the
/// record, not the server's rate, so both sides compute the same prediction whatever their clocks
/// say; at another rate the prediction is only less useful, and the server then codes the plain
/// residual instead (one bit tells which).
pub const PREDICTION_TICK_HZ: f64 = 60.0;

/// Bits a far record's x and z drop from the offset inside the cell (spec 120 § 8.3, ADR-10124):
/// a coarse step is `2^5` position steps, 3.2 cm at the default 1 mm.
pub const FAR_XZ_SHIFT: u32 = 5;

/// Bits a far record's y drops from the offset inside the cell: a coarse step is `2^10` position
/// steps, 1.024 m at the default 1 mm.
pub const FAR_Y_SHIFT: u32 = 10;

/// Bits of the absolute far position, `x u15 | z u15 | y u10`: the 5-byte record of spec 120 § 8.3.
pub const FAR_RECORD_BITS: u32 = 2 * (POSITION_CELL_BITS - FAR_XZ_SHIFT) + (POSITION_CELL_BITS - FAR_Y_SHIFT);

const CELL_STEPS: i64 = 1 << POSITION_CELL_BITS;
/// The wire order of a far record's axes (x, z, y) as indices into a position.
const FAR_AXES: [usize; 3] = [0, 2, 1];
/// The bits each axis of a position drops in a far record.
const FAR_SHIFTS: [u32; 3] = [FAR_XZ_SHIFT, FAR_Y_SHIFT, FAR_XZ_SHIFT];
/// Steps of one axis at most (`PositionQuantizer::steps` keeps `|n| <= i32::MAX`).
const MAX_STEPS: i64 = i32::MAX as i64;
const VELOCITY_BITS: u32 = 12;
const VELOCITY_MAX: i64 = (1 << (VELOCITY_BITS - 1)) - 1;
const ROTATION_BITS: u32 = 10;
const ROTATION_MAX: i64 = (1 << ROTATION_BITS) - 1;
/// Residual classes: payload bits of classes 0..=2 (class 3 is a varint).
const RESIDUAL_BITS: [u32; 3] = [4, 8, 14];

/// A position on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QPos {
    /// Raw `f32` bits (no grid: a child, a profile without a step, or out of the grid's range).
    Raw([u32; 3]),
    /// Whole steps of `step` (the `f32` bits of `position_step_m`).
    Grid {
        /// `position_step_m` as `f32` bits.
        step: u32,
        /// Steps per axis, `|n| <= i32::MAX`.
        n: [i64; 3],
    },
}

/// A smallest-three rotation on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QRot {
    largest: u8,
    kept: [u16; 3],
}

/// A velocity on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QVel {
    /// Steps of [`VELOCITY_STEP_MPS`], `|v| <= 2047`.
    Grid([i16; 3]),
    /// Raw `f32` bits (past the grid's range or not finite).
    Raw([u32; 3]),
}

/// A transform as the record carries it: integers both sides hold exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QTransform {
    pub(crate) mode: TransformReplication,
    pos: QPos,
    rot: Option<QRot>,
    vel: Option<QVel>,
    scale: Option<[u32; 3]>,
}

fn has_rotation(mode: TransformReplication) -> bool {
    matches!(mode, TransformReplication::Movement | TransformReplication::Full)
}

fn rotation_codec() -> Rotation {
    Rotation { bits: ROTATION_BITS }
}

fn bits3(v: Vec3) -> [u32; 3] {
    [v.x.to_bits(), v.y.to_bits(), v.z.to_bits()]
}

fn vec3(b: [u32; 3]) -> Vec3 {
    Vec3::new(f32::from_bits(b[0]), f32::from_bits(b[1]), f32::from_bits(b[2]))
}

fn grid_velocity(v: Vec3) -> Option<[i16; 3]> {
    let mut out = [0i16; 3];
    for (o, c) in out.iter_mut().zip(v.to_array()) {
        if !c.is_finite() {
            return None;
        }
        let n = (f64::from(c) / f64::from(VELOCITY_STEP_MPS)).round();
        if n.abs() > VELOCITY_MAX as f64 {
            return None;
        }
        *o = n as i16;
    }
    Some(out)
}

impl QTransform {
    /// The record of `t` under `mode`; `quant` is the root's position grid (`None`: raw).
    pub(crate) fn from_net(
        mode: TransformReplication,
        t: &NetTransform,
        quant: Option<PositionQuantizer>,
    ) -> Self {
        let p = t.translation;
        let pos = quant
            .and_then(|q| {
                Some(QPos::Grid {
                    step: q.step_m().to_bits(),
                    n: [
                        i64::from(q.steps(p.x)?),
                        i64::from(q.steps(p.y)?),
                        i64::from(q.steps(p.z)?),
                    ],
                })
            })
            .unwrap_or(QPos::Raw(bits3(p)));
        let moving = has_rotation(mode);
        let rot = moving.then(|| {
            let (largest, kept) = rotation_codec().quantize(t.rotation);
            QRot {
                largest: largest as u8,
                kept: kept.map(|k| k as u16),
            }
        });
        let vel = if moving {
            t.velocity
                .map(|v| grid_velocity(v).map_or(QVel::Raw(bits3(v)), QVel::Grid))
        } else {
            None
        };
        let scale = (mode == TransformReplication::Full && t.scale != Vec3::ONE).then(|| bits3(t.scale));
        Self {
            mode,
            pos,
            rot,
            vel,
            scale,
        }
    }

    /// The transform the integers stand for (what the client shows).
    pub(crate) fn to_net(self) -> NetTransform {
        let translation = match self.pos {
            QPos::Raw(b) => vec3(b),
            QPos::Grid { step, n } => {
                // `PositionQuantizer::value`: one rounding from f64, so both sides agree exactly.
                let s = f64::from(f32::from_bits(step));
                Vec3::new(
                    (n[0] as f64 * s) as f32,
                    (n[1] as f64 * s) as f32,
                    (n[2] as f64 * s) as f32,
                )
            }
        };
        let rotation = self.rot.map_or(Quat::IDENTITY, |r| {
            rotation_codec().dequantize(u64::from(r.largest), r.kept.map(u64::from))
        });
        let velocity = self.vel.map(|v| match v {
            QVel::Grid(g) => Vec3::new(
                f32::from(g[0]) * VELOCITY_STEP_MPS,
                f32::from(g[1]) * VELOCITY_STEP_MPS,
                f32::from(g[2]) * VELOCITY_STEP_MPS,
            ),
            QVel::Raw(b) => vec3(b),
        });
        NetTransform {
            translation,
            rotation,
            scale: self.scale.map_or(Vec3::ONE, vec3),
            velocity,
        }
    }

    /// The absolute form (the mode is written by the caller).
    pub(crate) fn write_abs(&self, w: &mut BitWriter) {
        write_pos(&self.pos, w);
        if has_rotation(self.mode) {
            write_rot(self.rot.unwrap_or(IDENTITY_ROT), w);
            write_vel(self.vel, w);
        }
        if self.mode == TransformReplication::Full {
            write_scale(self.scale, w);
        }
    }

    /// Reads the absolute form of `mode`.
    pub(crate) fn read_abs(mode: TransformReplication, r: &mut BitReader) -> Result<Self, DecodeError> {
        let pos = read_pos(r)?;
        let (rot, vel) = if has_rotation(mode) {
            (Some(read_rot(r)?), read_vel(r)?)
        } else {
            (None, None)
        };
        let scale = if mode == TransformReplication::Full {
            read_scale(r)?
        } else {
            None
        };
        Ok(Self {
            mode,
            pos,
            rot,
            vel,
            scale,
        })
    }

    /// Whether [`Self::write_delta`] can code `self` against `base` (the same mode).
    pub(crate) fn can_delta(&self, base: &Self) -> bool {
        self.mode == base.mode
    }

    /// The delta form against `base` (same mode, see [`Self::can_delta`]); `ticks` is how many
    /// server ticks this record is newer than `base` (the position predictor's time).
    pub(crate) fn write_delta(&self, base: &Self, ticks: u32, w: &mut BitWriter) {
        // Position.
        w.write_bool(self.pos != base.pos);
        if self.pos != base.pos {
            match (self.pos, base.pos) {
                (QPos::Grid { step, n }, QPos::Grid { step: bs, n: bn }) if step == bs => {
                    w.write_bool(true);
                    let mut from = bn;
                    if let Some(pred) = base.predicted_steps(ticks) {
                        // The cheaper of the two residuals: a walker's predicted residual is
                        // mostly zeros, a turn or a stop may favor the baseline itself.
                        let cost =
                            |r: [i64; 3]| n.iter().zip(r).map(|(a, b)| component_bits(a - b)).sum::<usize>();
                        let use_pred = cost(pred) < cost(bn);
                        w.write_bool(use_pred);
                        if use_pred {
                            from = pred;
                        }
                    }
                    for (a, b) in n.iter().zip(from) {
                        write_component(w, a - b);
                    }
                }
                _ => {
                    w.write_bool(false);
                    write_pos(&self.pos, w);
                }
            }
        }
        if has_rotation(self.mode) {
            let now = self.rot.unwrap_or(IDENTITY_ROT);
            let was = base.rot.unwrap_or(IDENTITY_ROT);
            w.write_bool(now != was);
            if now != was {
                w.write_bool(now.largest == was.largest);
                if now.largest == was.largest {
                    for (a, b) in now.kept.iter().zip(was.kept) {
                        write_component(w, i64::from(*a) - i64::from(b));
                    }
                } else {
                    write_rot(now, w);
                }
            }
            w.write_bool(self.vel != base.vel);
            if self.vel != base.vel {
                match (self.vel, base.vel) {
                    (Some(QVel::Grid(a)), Some(QVel::Grid(b))) => {
                        w.write_bool(true);
                        for (x, y) in a.iter().zip(b) {
                            write_component(w, i64::from(*x) - i64::from(y));
                        }
                    }
                    _ => {
                        w.write_bool(false);
                        write_vel(self.vel, w);
                    }
                }
            }
        }
        if self.mode == TransformReplication::Full {
            w.write_bool(self.scale != base.scale);
            if self.scale != base.scale {
                write_scale(self.scale, w);
            }
        }
    }

    /// Reads a delta against `base` written `ticks` server ticks after it; the result has the
    /// baseline's mode.
    pub(crate) fn read_delta(base: &Self, ticks: u32, r: &mut BitReader) -> Result<Self, DecodeError> {
        let mut out = *base;
        if r.read_bool()? {
            out.pos = if r.read_bool()? {
                let QPos::Grid { step, n } = base.pos else {
                    return Err(DecodeError::Invalid("position steps without a grid baseline"));
                };
                let mut from = n;
                if let Some(pred) = base.predicted_steps(ticks)
                    && r.read_bool()?
                {
                    from = pred;
                }
                let mut next = [0i64; 3];
                for (o, b) in next.iter_mut().zip(from) {
                    *o = b
                        .checked_add(read_component(r)?)
                        .filter(|v| v.abs() <= MAX_STEPS)
                        .ok_or(DecodeError::Invalid("position steps out of range"))?;
                }
                QPos::Grid { step, n: next }
            } else {
                read_pos(r)?
            };
        }
        if has_rotation(base.mode) {
            if r.read_bool()? {
                out.rot = Some(if r.read_bool()? {
                    let was = base.rot.unwrap_or(IDENTITY_ROT);
                    let mut kept = [0u16; 3];
                    for (o, b) in kept.iter_mut().zip(was.kept) {
                        let v = i64::from(b)
                            .checked_add(read_component(r)?)
                            .filter(|v| (0..=ROTATION_MAX).contains(v))
                            .ok_or(DecodeError::Invalid("rotation component out of range"))?;
                        *o = v as u16;
                    }
                    QRot {
                        largest: was.largest,
                        kept,
                    }
                } else {
                    read_rot(r)?
                });
            }
            if r.read_bool()? {
                out.vel = if r.read_bool()? {
                    let Some(QVel::Grid(was)) = base.vel else {
                        return Err(DecodeError::Invalid("velocity steps without a grid baseline"));
                    };
                    let mut g = [0i16; 3];
                    for (o, b) in g.iter_mut().zip(was) {
                        let v = i64::from(b)
                            .checked_add(read_component(r)?)
                            .filter(|v| (-VELOCITY_MAX - 1..=VELOCITY_MAX).contains(v))
                            .ok_or(DecodeError::Invalid("velocity component out of range"))?;
                        *o = v as i16;
                    }
                    Some(QVel::Grid(g))
                } else {
                    read_vel(r)?
                };
            }
        }
        if base.mode == TransformReplication::Full && r.read_bool()? {
            out.scale = read_scale(r)?;
        }
        Ok(out)
    }
}

/// A grid position as a far record sees it: the cell of each axis and the offset inside it
/// rounded to the far record's coarse steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FarPos {
    step: u32,
    cell: [i64; 3],
    coarse: [i64; 3],
}

impl QTransform {
    /// The position as a far record carries it: `None` for a raw position, and for an offset that
    /// rounds up out of its cell (the last half coarse step of a cell: 1.6 cm of every 1,048 m on
    /// x and z, 0.51 m on y), where the exact record goes instead. Rounding to the nearest coarse
    /// step keeps a position on the coarse grid (the origin, a floor at height 0) exact.
    pub(crate) fn far_pos(&self) -> Option<FarPos> {
        let QPos::Grid { step, n } = self.pos else {
            return None;
        };
        let mut cell = [0i64; 3];
        let mut coarse = [0i64; 3];
        for (((cell, coarse), n), shift) in cell.iter_mut().zip(&mut coarse).zip(n).zip(FAR_SHIFTS) {
            *cell = n.div_euclid(CELL_STEPS);
            let c = (n.rem_euclid(CELL_STEPS) + (1 << (shift - 1))) >> shift;
            // Rounded up out of the cell, or (the lowest cell's first steps) below the grid.
            if c >= 1 << (POSITION_CELL_BITS - shift) || (*cell * CELL_STEPS + (c << shift)).abs() > MAX_STEPS
            {
                return None;
            }
            *coarse = c;
        }
        Some(FarPos { step, cell, coarse })
    }

    /// Whether a far record can stand for `self` against `base`: both on one grid and in one cell
    /// on every axis, the same mode, and `self` has a coarse position.
    pub(crate) fn far_against(&self, base: &Self) -> Option<FarPos> {
        let far = self.far_pos()?;
        let QPos::Grid { step, n } = base.pos else {
            return None;
        };
        let same_cell = (0..3).all(|axis| n[axis].div_euclid(CELL_STEPS) == far.cell[axis]);
        (self.mode == base.mode && step == far.step && same_cell).then_some(far)
    }

    /// Writes `far` (`self.far_against(base)`) as a far record: the residual form against the
    /// baseline's coarse position when the baseline has one and the form is shorter, else the
    /// absolute 40 bits.
    pub(crate) fn write_far(far: &FarPos, base: &Self, w: &mut BitWriter) {
        let residual = base.far_pos().filter(|b| {
            let bits: usize = FAR_AXES
                .iter()
                .map(|&axis| component_bits(far.coarse[axis] - b.coarse[axis]))
                .sum();
            bits < FAR_RECORD_BITS as usize
        });
        w.write_bool(residual.is_none());
        match residual {
            Some(b) => {
                for axis in FAR_AXES {
                    write_component(w, far.coarse[axis] - b.coarse[axis]);
                }
            }
            None => {
                for axis in FAR_AXES {
                    w.write_bits(far.coarse[axis] as u64, POSITION_CELL_BITS - FAR_SHIFTS[axis]);
                }
            }
        }
    }

    /// Reads a far record against `base`: the baseline's mode, rotation and scale, no velocity,
    /// and the coarse position in the baseline's cell. Whatever the bytes, the position stays
    /// inside that cell, so it is always a valid grid position.
    pub(crate) fn read_far(base: &Self, r: &mut BitReader) -> Result<Self, DecodeError> {
        let QPos::Grid { step, n } = base.pos else {
            return Err(DecodeError::Invalid("far record without a grid baseline"));
        };
        let absolute = r.read_bool()?;
        let from = if absolute {
            None
        } else {
            Some(
                base.far_pos()
                    .ok_or(DecodeError::Invalid("far residual without a coarse baseline"))?,
            )
        };
        let mut next = n;
        for axis in FAR_AXES {
            let shift = FAR_SHIFTS[axis];
            let bits = POSITION_CELL_BITS - shift;
            let coarse = match &from {
                None => r.read_bits(bits)? as i64,
                Some(b) => b.coarse[axis]
                    .checked_add(read_component(r)?)
                    .filter(|c| (0..1i64 << bits).contains(c))
                    .ok_or(DecodeError::Invalid("far position out of its cell"))?,
            };
            next[axis] = n[axis].div_euclid(CELL_STEPS) * CELL_STEPS + (coarse << shift);
            if next[axis].abs() > MAX_STEPS {
                return Err(DecodeError::Invalid("position steps out of range"));
            }
        }
        Ok(Self {
            mode: base.mode,
            pos: QPos::Grid { step, n: next },
            rot: base.rot,
            vel: None,
            scale: base.scale,
        })
    }

    /// A far position shown on a copy whose newest applied record is `shown`, newer than the far
    /// record's baseline: the rotation and scale stay as shown (they are display state only; a far
    /// version is never a transform baseline). Another mode keeps the baseline's.
    pub(crate) fn carrying(mut self, shown: Option<&Self>) -> Self {
        if let Some(shown) = shown.filter(|s| s.mode == self.mode) {
            self.rot = shown.rot;
            self.scale = shown.scale;
        }
        self
    }

    /// The transform a client shows after a far record of `self` against `base`
    /// (`self.far_against(base)` holds): what [`Self::read_far`] returns.
    #[cfg(test)]
    pub(crate) fn far_shown(&self, base: &Self) -> Option<Self> {
        let far = self.far_against(base)?;
        let n =
            std::array::from_fn(|axis| far.cell[axis] * CELL_STEPS + (far.coarse[axis] << FAR_SHIFTS[axis]));
        Some(Self {
            mode: base.mode,
            pos: QPos::Grid { step: far.step, n },
            rot: base.rot,
            vel: None,
            scale: base.scale,
        })
    }
}

impl QTransform {
    /// The baseline's grid position moved along its grid velocity for `ticks` ticks at
    /// [`PREDICTION_TICK_HZ`] (ADR-8522), in whole steps: `None` when there is nothing to predict
    /// (no grid position or grid velocity, no time, or a prediction past the grid's range). Both
    /// sides compute it from the same integers with the same `f64` operations (IEEE-754 basic
    /// operations and `round` are exact functions of their inputs), so they agree bit for bit.
    fn predicted_steps(&self, ticks: u32) -> Option<[i64; 3]> {
        let (QPos::Grid { step, n }, Some(QVel::Grid(g))) = (self.pos, self.vel) else {
            return None;
        };
        if ticks == 0 || g == [0; 3] {
            return None;
        }
        let step = f64::from(f32::from_bits(step));
        // Metres per velocity step and tick, over metres per position step.
        let scale = f64::from(VELOCITY_STEP_MPS) * f64::from(ticks) / PREDICTION_TICK_HZ / step;
        let mut out = n;
        for (o, v) in out.iter_mut().zip(g) {
            let moved = (f64::from(v) * scale).round();
            if !moved.is_finite() || moved.abs() > MAX_STEPS as f64 {
                return None;
            }
            *o = o.checked_add(moved as i64).filter(|x| x.abs() <= MAX_STEPS)?;
        }
        Some(out)
    }

    /// How far `now` is from this record extrapolated `seconds` along its velocity (what a client
    /// holding this record shows, spec 120 § 8.2), in metres, and how far its rotation turned, in
    /// degrees. The extrapolation gate (ADR-8522) compares these with the profile's gate.
    pub(crate) fn extrapolation_error(&self, now: &Self, seconds: f32) -> (f32, f32) {
        let held = self.to_net();
        let shown =
            held.translation + held.velocity.filter(|v| v.is_finite()).unwrap_or(Vec3::ZERO) * seconds;
        let truth = now.to_net();
        let turned = if has_rotation(self.mode) && self.rot != now.rot {
            held.rotation.angle_between(truth.rotation).to_degrees()
        } else {
            0.0
        };
        (shown.distance(truth.translation), turned)
    }

    /// Whether only the position, rotation and velocity differ from `base` (the parts the
    /// extrapolation gate may hold back): the same mode, position grid and scale.
    pub(crate) fn gate_comparable(&self, base: &Self) -> bool {
        let same_grid = match (self.pos, base.pos) {
            (QPos::Grid { step, .. }, QPos::Grid { step: bs, .. }) => step == bs,
            (QPos::Raw(_), QPos::Raw(_)) => true,
            _ => false,
        };
        self.mode == base.mode && same_grid && self.scale == base.scale
    }
}

const IDENTITY_ROT: QRot = QRot {
    largest: 3,
    kept: [512, 512, 512],
};

fn write_pos(pos: &QPos, w: &mut BitWriter) {
    match *pos {
        QPos::Grid { step, n } => {
            w.write_bool(true);
            let default = step == DEFAULT_WIRE_STEP_M.to_bits();
            w.write_bool(default);
            if !default {
                w.write_bits(u64::from(step), 32);
            }
            for a in n {
                w.write_zigzag(a.div_euclid(CELL_STEPS));
                w.write_bits(a.rem_euclid(CELL_STEPS) as u64, POSITION_CELL_BITS);
            }
        }
        QPos::Raw(b) => {
            w.write_bool(false);
            for x in b {
                w.write_bits(u64::from(x), 32);
            }
        }
    }
}

fn read_pos(r: &mut BitReader) -> Result<QPos, DecodeError> {
    if !r.read_bool()? {
        return Ok(QPos::Raw([
            r.read_bits(32)? as u32,
            r.read_bits(32)? as u32,
            r.read_bits(32)? as u32,
        ]));
    }
    let step = if r.read_bool()? {
        DEFAULT_WIRE_STEP_M.to_bits()
    } else {
        r.read_bits(32)? as u32
    };
    let s = f32::from_bits(step);
    if !(s.is_finite() && s > 0.0) {
        return Err(DecodeError::Invalid("position step"));
    }
    let mut n = [0i64; 3];
    for o in &mut n {
        let cell = r.read_zigzag()?;
        let offset = r.read_bits(POSITION_CELL_BITS)? as i64;
        *o = cell
            .checked_mul(CELL_STEPS)
            .and_then(|c| c.checked_add(offset))
            .filter(|v| v.abs() <= MAX_STEPS)
            .ok_or(DecodeError::Invalid("position cell out of range"))?;
    }
    Ok(QPos::Grid { step, n })
}

fn write_rot(rot: QRot, w: &mut BitWriter) {
    w.write_bits(u64::from(rot.largest), 2);
    for k in rot.kept {
        w.write_bits(u64::from(k), ROTATION_BITS);
    }
}

fn read_rot(r: &mut BitReader) -> Result<QRot, DecodeError> {
    let largest = r.read_bits(2)? as u8;
    let mut kept = [0u16; 3];
    for k in &mut kept {
        *k = r.read_bits(ROTATION_BITS)? as u16;
    }
    Ok(QRot { largest, kept })
}

fn write_vel(vel: Option<QVel>, w: &mut BitWriter) {
    w.write_bool(vel.is_some());
    match vel {
        Some(QVel::Grid(g)) => {
            w.write_bool(true);
            for c in g {
                w.write_bits((i64::from(c) as u64) & ((1 << VELOCITY_BITS) - 1), VELOCITY_BITS);
            }
        }
        Some(QVel::Raw(b)) => {
            w.write_bool(false);
            for x in b {
                w.write_bits(u64::from(x), 32);
            }
        }
        None => {}
    }
}

fn read_vel(r: &mut BitReader) -> Result<Option<QVel>, DecodeError> {
    if !r.read_bool()? {
        return Ok(None);
    }
    if r.read_bool()? {
        let mut g = [0i16; 3];
        for c in &mut g {
            let raw = r.read_bits(VELOCITY_BITS)? as i64;
            // Sign-extend the 12-bit two's complement value.
            *c = (if raw > VELOCITY_MAX {
                raw - (1 << VELOCITY_BITS)
            } else {
                raw
            }) as i16;
        }
        return Ok(Some(QVel::Grid(g)));
    }
    Ok(Some(QVel::Raw([
        r.read_bits(32)? as u32,
        r.read_bits(32)? as u32,
        r.read_bits(32)? as u32,
    ])))
}

fn write_scale(scale: Option<[u32; 3]>, w: &mut BitWriter) {
    w.write_bool(scale.is_some());
    if let Some(b) = scale {
        for x in b {
            w.write_bits(u64::from(x), 32);
        }
    }
}

fn read_scale(r: &mut BitReader) -> Result<Option<[u32; 3]>, DecodeError> {
    if !r.read_bool()? {
        return Ok(None);
    }
    Ok(Some([
        r.read_bits(32)? as u32,
        r.read_bits(32)? as u32,
        r.read_bits(32)? as u32,
    ]))
}

/// The bits [`write_component`] spends on `d`.
fn component_bits(d: i64) -> usize {
    if d == 0 {
        return 1;
    }
    let z = ((d << 1) ^ (d >> 63)) as u64 - 1;
    for bits in RESIDUAL_BITS {
        if z < (1u64 << bits) {
            return 3 + bits as usize;
        }
    }
    3 + crate::bits::varint_bits(z)
}

/// One component's change bit and, when it changed, its residual.
fn write_component(w: &mut BitWriter, d: i64) {
    w.write_bool(d != 0);
    if d == 0 {
        return;
    }
    // zigzag(d) >= 1 for d != 0, so the residual codes zigzag(d) - 1 from 0.
    let z = ((d << 1) ^ (d >> 63)) as u64 - 1;
    for (class, bits) in RESIDUAL_BITS.iter().enumerate() {
        if z < (1u64 << bits) {
            w.write_bits(class as u64, 2);
            w.write_bits(z, *bits);
            return;
        }
    }
    w.write_bits(3, 2);
    w.write_varint(z);
}

/// Reads one component's change bit and residual (0 when unchanged).
fn read_component(r: &mut BitReader) -> Result<i64, DecodeError> {
    if !r.read_bool()? {
        return Ok(0);
    }
    let class = r.read_bits(2)? as usize;
    let z = match RESIDUAL_BITS.get(class) {
        Some(bits) => r.read_bits(*bits)?,
        None => r.read_varint()?,
    };
    let zz = z.checked_add(1).ok_or(DecodeError::Invalid("residual"))?;
    Ok(((zz >> 1) as i64) ^ -((zz & 1) as i64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn walker(p: Vec3, yaw: f32, v: Vec3) -> NetTransform {
        NetTransform {
            translation: p,
            rotation: Quat::from_rotation_y(yaw),
            scale: Vec3::ONE,
            velocity: Some(v),
        }
    }

    fn mm() -> Option<PositionQuantizer> {
        PositionQuantizer::new(DEFAULT_WIRE_STEP_M)
    }

    fn abs_round_trip(q: &QTransform) -> QTransform {
        let mut w = BitWriter::new();
        q.write_abs(&mut w);
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        QTransform::read_abs(q.mode, &mut r).expect("decode")
    }

    fn delta_round_trip(q: &QTransform, base: &QTransform) -> (QTransform, usize) {
        delta_round_trip_after(q, base, 0)
    }

    /// A delta written `ticks` after its baseline (the predictor's time).
    fn delta_round_trip_after(q: &QTransform, base: &QTransform, ticks: u32) -> (QTransform, usize) {
        let mut w = BitWriter::new();
        q.write_delta(base, ticks, &mut w);
        let bits = w.bit_len();
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        (QTransform::read_delta(base, ticks, &mut r).expect("decode"), bits)
    }

    #[test]
    fn the_absolute_record_round_trips_exactly_and_within_the_step() {
        let modes = [
            TransformReplication::PositionOnly,
            TransformReplication::Movement,
            TransformReplication::Full,
        ];
        let mut seed = 0x8467u64;
        let mut next = || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((seed >> 11) as f64 / (1u64 << 53) as f64) as f32
        };
        for i in 0..2000 {
            let mode = modes[i % 3];
            let p = Vec3::new(
                (next() - 0.5) * 4000.0,
                (next() - 0.5) * 200.0,
                (next() - 0.5) * 4000.0,
            );
            let t = NetTransform {
                translation: p,
                rotation: Quat::from_euler(glam::EulerRot::YXZ, next() * 6.0, next() - 0.5, next() - 0.5),
                scale: if i % 7 == 0 { Vec3::splat(2.0) } else { Vec3::ONE },
                velocity: (i % 5 != 0).then(|| Vec3::new(next() - 0.5, next() - 0.5, next() - 0.5) * 20.0),
            };
            let q = QTransform::from_net(mode, &t, mm());
            assert_eq!(abs_round_trip(&q), q, "integers survive the wire");
            let back = q.to_net();
            for axis in 0..3 {
                let err = (back.translation[axis] - p[axis]).abs();
                let bound = 0.5 * DEFAULT_WIRE_STEP_M + 0.5 * f32::EPSILON * p[axis].abs().max(1.0);
                assert!(err <= bound * 1.0001, "axis {axis}: {err} > {bound}");
            }
            if mode != TransformReplication::PositionOnly {
                assert!(back.rotation.angle_between(t.rotation).to_degrees() < 0.2);
                if let (Some(a), Some(b)) = (back.velocity, t.velocity) {
                    assert!((a - b).abs().max_element() <= 0.5 * VELOCITY_STEP_MPS + 1e-6);
                }
            }
        }
    }

    /// Spec 120 § 8.2 sizes: the absolute record of a moving root at the default precision is
    /// 2 + 3 x (8 + 20) position bits, 32 rotation bits and 2 + 36 velocity bits (19 bytes, beside
    /// the 18-byte DO27 record); a walker's delta against a baseline a round trip old is under 4.
    #[test]
    fn record_sizes_at_the_default_precision() {
        let base = walker(Vec3::new(123.4, 1.5, -321.7), 0.7, Vec3::new(0.9, 0.0, 1.1));
        let q = QTransform::from_net(TransformReplication::Movement, &base, mm());
        let mut w = BitWriter::new();
        q.write_abs(&mut w);
        assert_eq!(w.bit_len(), 2 + 3 * (8 + 20) + 32 + 2 + 36);
        // Six ticks at 60 Hz later, the same heading and pace.
        let later = walker(
            base.translation + Vec3::new(0.9, 0.0, 1.1) * 0.1,
            0.7,
            Vec3::new(0.9, 0.0, 1.1),
        );
        let now = QTransform::from_net(TransformReplication::Movement, &later, mm());
        let (back, bits) = delta_round_trip(&now, &q);
        assert_eq!(back, now);
        // pos: 1 + 1 + (1 + 2 + 8) x 2 + 1; rot and velocity unchanged: 1 + 1.
        assert_eq!(bits, 1 + 1 + 2 * (1 + 2 + 8) + 1 + 2, "{bits}");
        let (_, same_bits) = delta_round_trip(&q, &q);
        assert_eq!(same_bits, 3, "nothing changed: one bit per part");
    }

    #[test]
    fn deltas_reconstruct_every_change_exactly() {
        let base_t = walker(Vec3::new(10.0, 2.0, -5.0), 0.0, Vec3::new(1.0, 0.0, 0.0));
        let base = QTransform::from_net(TransformReplication::Full, &base_t, mm());
        let cases = [
            // A turn: the rotation's largest component stays.
            walker(Vec3::new(10.1, 2.0, -5.0), 0.3, Vec3::new(1.0, 0.0, 0.3)),
            // A turn past 90 degrees: another largest component.
            walker(Vec3::new(10.1, 2.0, -5.0), 2.9, Vec3::new(-1.0, 0.0, 0.3)),
            // A cell crossing and a long jump.
            walker(Vec3::new(1048.0, 2.0, -5000.0), 0.0, Vec3::new(1.0, 0.0, 0.0)),
            // Velocity past the grid (raw) and back.
            walker(Vec3::new(10.0, 2.0, -5.0), 0.0, Vec3::new(300.0, 0.0, 0.0)),
            // A scale and no velocity.
            NetTransform {
                translation: Vec3::new(10.0, 2.0, -5.0),
                rotation: Quat::IDENTITY,
                scale: Vec3::new(1.0, 2.0, 1.0),
                velocity: None,
            },
        ];
        for t in cases {
            let now = QTransform::from_net(TransformReplication::Full, &t, mm());
            assert_eq!(delta_round_trip(&now, &base).0, now, "{t:?}");
            assert_eq!(delta_round_trip(&base, &now).0, base, "back from {t:?}");
        }
        // A raw position (another grid or none) against a grid baseline, and the other way.
        let raw = QTransform::from_net(TransformReplication::Full, &base_t, None);
        assert_eq!(delta_round_trip(&raw, &base).0, raw);
        assert_eq!(delta_round_trip(&base, &raw).0, base);
        let coarse = QTransform::from_net(TransformReplication::Full, &base_t, PositionQuantizer::new(0.25));
        assert_eq!(delta_round_trip(&coarse, &base).0, coarse);
    }

    #[test]
    fn residual_classes_cover_the_whole_range() {
        for d in [
            1i64,
            -1,
            8,
            -8,
            9,
            128,
            -129,
            8191,
            -8192,
            8193,
            1 << 31,
            -(1 << 31),
            i64::MAX / 2,
            i64::MIN / 2,
        ] {
            let mut w = BitWriter::new();
            write_component(&mut w, d);
            let bytes = w.finish();
            let mut r = BitReader::new(&bytes);
            assert_eq!(read_component(&mut r).expect("read"), d);
        }
    }

    #[test]
    fn hostile_records_are_errors_not_panics() {
        let base = QTransform::from_net(
            TransformReplication::Full,
            &walker(Vec3::new(1.0, 2.0, 3.0), 0.5, Vec3::new(1.0, 0.0, 0.0)),
            mm(),
        );
        let raw_base = QTransform::from_net(
            TransformReplication::Full,
            &walker(Vec3::new(1.0, 2.0, 3.0), 0.5, Vec3::new(500.0, 0.0, 0.0)),
            None,
        );
        let mut state = 7u64;
        for len in 0..64usize {
            for _ in 0..64 {
                let bytes: Vec<u8> = (0..len)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        state as u8
                    })
                    .collect();
                for mode in [
                    TransformReplication::None,
                    TransformReplication::PositionOnly,
                    TransformReplication::Movement,
                    TransformReplication::Full,
                ] {
                    let _ = QTransform::read_abs(mode, &mut BitReader::new(&bytes)).map(|q| q.to_net());
                }
                for b in [&base, &raw_base] {
                    for ticks in [0, 6, u32::MAX] {
                        if let Ok(q) = QTransform::read_delta(b, ticks, &mut BitReader::new(&bytes)) {
                            let _ = q.to_net();
                        }
                    }
                }
            }
        }
        // Steps past the grid's range are refused.
        let mut w = BitWriter::new();
        w.write_bool(true); // grid
        w.write_bool(true); // default step
        w.write_zigzag(1 << 20); // a cell far past i32 steps
        w.write_bits(0, POSITION_CELL_BITS);
        let bytes = w.finish();
        assert!(read_pos(&mut BitReader::new(&bytes)).is_err());
        // A zero step is refused.
        let mut w = BitWriter::new();
        w.write_bool(true);
        w.write_bool(false);
        w.write_bits(0, 32);
        let bytes = w.finish();
        assert!(read_pos(&mut BitReader::new(&bytes)).is_err());
    }

    /// ADR-8522: a walker's position residual is coded against its baseline moved along the
    /// baseline's velocity. Six ticks on at the same pace, the residual is zero on every axis
    /// (the change bit, the predicted bit and three zero component bits), and every case still
    /// decodes to exactly the server's integers.
    #[test]
    fn a_predicted_residual_is_smaller_and_exact() {
        let v = Vec3::new(0.9, 0.0, 1.1);
        let base = QTransform::from_net(
            TransformReplication::Movement,
            &walker(Vec3::new(123.4, 1.5, -321.7), 0.7, v),
            mm(),
        );
        // The grid velocity is what the baseline carries: move the walker by exactly that.
        let vq = base.to_net().velocity.expect("velocity");
        let later = walker(base.to_net().translation + vq * 0.1, 0.7, v);
        let now = QTransform::from_net(TransformReplication::Movement, &later, mm());
        let (back, bits) = delta_round_trip_after(&now, &base, 6);
        assert_eq!(back, now);
        // pos: changed, grid, predicted, 3 x unchanged; rotation and velocity unchanged.
        assert_eq!(bits, 1 + 1 + 1 + 3 + 2, "{bits}");
        let (_, plain_bits) = delta_round_trip_after(&now, &base, 0);
        assert!(plain_bits > bits + 10, "plain {plain_bits} vs predicted {bits}");

        // A stop, a turn, a jump, another grid, raw velocities: exact whatever the server picked.
        let cases = [
            walker(base.to_net().translation, 0.7, Vec3::ZERO),
            walker(
                base.to_net().translation + Vec3::new(-0.2, 0.0, 0.05),
                2.0,
                Vec3::new(-1.0, 0.0, 0.3),
            ),
            walker(Vec3::new(5000.0, -20.0, 1048.6), 0.7, v),
            walker(base.to_net().translation, 0.7, Vec3::new(300.0, 0.0, 0.0)),
        ];
        for ticks in [1u32, 6, 60, 600, u32::MAX] {
            for t in &cases {
                let now = QTransform::from_net(TransformReplication::Movement, t, mm());
                assert_eq!(
                    delta_round_trip_after(&now, &base, ticks).0,
                    now,
                    "{ticks}: {t:?}"
                );
                assert_eq!(
                    delta_round_trip_after(&base, &now, ticks).0,
                    base,
                    "back, {ticks}: {t:?}"
                );
            }
        }
        // A baseline at rest or without a grid velocity has nothing to predict: no extra bit.
        let still = QTransform::from_net(
            TransformReplication::Movement,
            &walker(Vec3::new(1.0, 2.0, 3.0), 0.0, Vec3::ZERO),
            mm(),
        );
        assert_eq!(still.predicted_steps(6), None);
        let moved = QTransform::from_net(
            TransformReplication::Movement,
            &walker(Vec3::new(1.1, 2.0, 3.0), 0.0, Vec3::ZERO),
            mm(),
        );
        assert_eq!(
            delta_round_trip_after(&moved, &still, 6).1,
            delta_round_trip_after(&moved, &still, 0).1
        );
        // A prediction past the grid's range is not made.
        let edge = QTransform::from_net(
            TransformReplication::Movement,
            &walker(Vec3::new(2_147_000.0, 0.0, 0.0), 0.0, Vec3::new(60.0, 0.0, 0.0)),
            mm(),
        );
        assert_eq!(edge.predicted_steps(u32::MAX), None);
    }

    /// The gate's error model (ADR-8522): the held record moved along its velocity, against the
    /// server's record; the rotation's turn in degrees.
    #[test]
    fn the_extrapolation_error_follows_the_velocity() {
        let v = Vec3::new(1.0, 0.0, 0.0);
        let held = QTransform::from_net(TransformReplication::Movement, &walker(Vec3::ZERO, 0.0, v), mm());
        let on_track = QTransform::from_net(
            TransformReplication::Movement,
            &walker(Vec3::new(0.5, 0.0, 0.0), 0.0, v),
            mm(),
        );
        let (d, turned) = held.extrapolation_error(&on_track, 0.5);
        assert!(d < 0.002, "{d}");
        assert_eq!(turned, 0.0);
        let off = QTransform::from_net(
            TransformReplication::Movement,
            &walker(Vec3::new(0.5, 0.0, 0.03), 0.02, v),
            mm(),
        );
        let (d, turned) = held.extrapolation_error(&off, 0.5);
        assert!((d - 0.03).abs() < 0.002, "{d}");
        assert!((turned - 0.02f32.to_degrees()).abs() < 0.2, "{turned}");
        assert!(off.gate_comparable(&held));
        let raw = QTransform::from_net(TransformReplication::Movement, &walker(Vec3::ZERO, 0.0, v), None);
        assert!(
            !raw.gate_comparable(&held),
            "another position grid is not comparable"
        );
    }

    /// Positions out of the grid's range, non-finite values and children travel raw.
    #[test]
    fn values_off_the_grid_travel_raw() {
        let far = walker(Vec3::new(3.0e6, 0.0, 0.0), 0.0, Vec3::new(f32::NAN, 0.0, 0.0));
        let q = QTransform::from_net(TransformReplication::Movement, &far, mm());
        assert!(matches!(q.pos, QPos::Raw(_)));
        assert!(matches!(q.vel, Some(QVel::Raw(_))));
        let back = abs_round_trip(&q).to_net();
        assert_eq!(back.translation, far.translation);
        assert!(back.velocity.is_some_and(|v| v.x.is_nan()));
    }
    fn far_round_trip(q: &QTransform, base: &QTransform) -> Option<(QTransform, usize)> {
        let far = q.far_against(base)?;
        let mut w = BitWriter::new();
        QTransform::write_far(&far, base, &mut w);
        let bits = w.bit_len();
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        let back = QTransform::read_far(base, &mut r).expect("decode");
        assert_eq!(r.bit_pos(), bits, "the reader used what the writer wrote");
        Some((back, bits))
    }

    /// Spec 120 § 8.3 (ADR-10124): the far record round-trips bit for bit to the coarse position
    /// the server computed, within half a coarse step of the exact one (1.6 cm on x and z, 0.512 m
    /// on y at 1 mm), whatever baseline of the same cell it is coded against; the absolute form
    /// is 1 + 40 bits and the residual form is never longer.
    #[test]
    fn the_far_record_round_trips_exactly_within_half_a_coarse_step() {
        let mut seed = 0x1_0124u64;
        let mut next = || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((seed >> 11) as f64 / (1u64 << 53) as f64) as f32
        };
        let (mut far_records, mut residual_forms) = (0u32, 0u32);
        for i in 0..4000 {
            let mode = [
                TransformReplication::PositionOnly,
                TransformReplication::Movement,
                TransformReplication::Full,
            ][i % 3];
            // Both signs, several cells; every fourth pair a short walk apart.
            let p = Vec3::new(
                (next() - 0.5) * 6000.0,
                (next() - 0.5) * 3000.0,
                (next() - 0.5) * 6000.0,
            );
            let reach = if i % 4 == 0 { 2.0 } else { 900.0 };
            let b = p + Vec3::new(next() - 0.5, next() - 0.5, next() - 0.5) * reach;
            let now = QTransform::from_net(mode, &walker(p, next() * 6.0, Vec3::new(1.0, 0.0, next())), mm());
            let base =
                QTransform::from_net(mode, &walker(b, next() * 6.0, Vec3::new(next(), 0.0, 1.0)), mm());
            let Some((back, bits)) = far_round_trip(&now, &base) else {
                continue;
            };
            far_records += 1;
            residual_forms += u32::from(bits != 1 + FAR_RECORD_BITS as usize);
            assert!(bits <= 1 + FAR_RECORD_BITS as usize, "{bits}");
            assert_eq!(
                Some(back),
                now.far_shown(&base),
                "the client holds the server's coarse integers"
            );
            // The position is a function of the server's position alone: the same from the far
            // form of the baseline, and coded against it the record is the same bits.
            // (A baseline in the last half coarse step of its cell has no far form.)
            if let Some(far_base) = base.far_shown(&base) {
                let (again, again_bits) = far_round_trip(&now, &far_base).expect("same cell");
                assert_eq!(again.pos, back.pos);
                assert_eq!(
                    again_bits, bits,
                    "either form of the baseline gives the same record"
                );
            }
            let (QPos::Grid { n: exact, .. }, QPos::Grid { n: shown, .. }) = (now.pos, back.pos) else {
                panic!("grid positions");
            };
            for (axis, half) in [(0, 16), (1, 512), (2, 16)] {
                assert!(
                    (exact[axis] - shown[axis]).abs() <= half,
                    "axis {axis}: {} vs {}",
                    exact[axis],
                    shown[axis]
                );
                assert_eq!(
                    exact[axis].div_euclid(CELL_STEPS),
                    shown[axis].div_euclid(CELL_STEPS),
                    "same cell"
                );
            }
            assert_eq!(back.mode, base.mode);
            assert_eq!(back.rot, base.rot, "the rotation is the baseline's");
            assert_eq!(back.vel, None, "no velocity");
            assert_eq!(back.scale, base.scale);
            // The shown position in metres: 16 and 512 steps of 1 mm, plus what two `f32` values
            // 3 km out lose to rounding (0.24 mm each).
            let (a, s) = (now.to_net().translation, back.to_net().translation);
            let (xz, y) = (0.016 + 6e-4, 0.512 + 6e-4);
            assert!(
                (a.x - s.x).abs() <= xz && (a.z - s.z).abs() <= xz && (a.y - s.y).abs() <= y,
                "{a} vs {s}"
            );
        }
        assert!(far_records > 1000, "{far_records} pairs shared a cell");
        assert!(residual_forms > 200, "{residual_forms} residual forms");
    }

    /// The far record's sizes: 41 bits absolute; 24 bits for a walker half a second on (0.7 m: the
    /// form bit, two residuals of 11 bits and an unchanged y); 4 bits for a far copy that did not
    /// leave its coarse step. A position on the coarse grid (the origin) is exact.
    #[test]
    fn far_record_sizes_and_exact_grid_points() {
        let at =
            |p: Vec3| QTransform::from_net(TransformReplication::Movement, &walker(p, 0.7, Vec3::X), mm());
        let base = at(Vec3::new(123.4, 1.5, -321.7));
        let (_, bits) = far_round_trip(&at(Vec3::new(123.9, 1.5, -321.2)), &base).expect("far");
        assert_eq!(bits, 1 + 2 * (1 + 2 + 8) + 1, "a walker's residual");
        let (_, bits) = far_round_trip(&at(Vec3::new(123.405, 1.52, -321.705)), &base).expect("far");
        assert_eq!(bits, 4, "the same coarse position");
        let (_, bits) = far_round_trip(&at(Vec3::new(900.0, 700.0, -900.0)), &base).expect("far");
        assert_eq!(
            bits,
            1 + FAR_RECORD_BITS as usize,
            "a jump inside the cell is absolute"
        );
        assert_eq!(FAR_RECORD_BITS, 40);
        let origin = at(Vec3::ZERO);
        let (back, _) = far_round_trip(&origin, &at(Vec3::new(3.0, 0.2, 4.0))).expect("far");
        assert_eq!(back.to_net().translation, Vec3::ZERO);
        let grid = at(Vec3::new(0.064, 2.048, 0.32));
        let (back, _) = far_round_trip(&grid, &origin).expect("far");
        assert_eq!(back.pos, grid.pos, "multiples of the coarse steps are exact");
    }

    /// Where the far record does not apply: another cell on any axis, another grid or a raw
    /// position, another mode, and an offset that rounds up out of its cell.
    #[test]
    fn the_far_record_declines_what_it_cannot_carry() {
        let at =
            |p: Vec3| QTransform::from_net(TransformReplication::Movement, &walker(p, 0.0, Vec3::X), mm());
        let base = at(Vec3::new(10.0, 2.0, -5.0));
        assert!(at(Vec3::new(11.0, 2.0, -5.5)).far_against(&base).is_some());
        // x crosses into the next cell (a cell is 1,048.576 m); z of the baseline is in cell -1.
        assert!(at(Vec3::new(1049.0, 2.0, -5.0)).far_against(&base).is_none());
        assert!(at(Vec3::new(10.0, 2.0, 5.0)).far_against(&base).is_none());
        assert!(at(Vec3::new(10.0, -2.0, -5.0)).far_against(&base).is_none());
        let raw = QTransform::from_net(
            TransformReplication::Movement,
            &walker(Vec3::new(10.0, 2.0, -5.0), 0.0, Vec3::X),
            None,
        );
        assert!(raw.far_against(&base).is_none() && base.far_against(&raw).is_none());
        let coarse = QTransform::from_net(
            TransformReplication::Movement,
            &walker(Vec3::new(10.0, 2.0, -5.0), 0.0, Vec3::X),
            PositionQuantizer::new(0.25),
        );
        assert!(coarse.far_against(&base).is_none(), "another step");
        let full = QTransform::from_net(
            TransformReplication::Full,
            &walker(Vec3::new(10.0, 2.0, -5.0), 0.0, Vec3::X),
            mm(),
        );
        assert!(full.far_against(&base).is_none(), "another mode");
        // The last half coarse step of a cell rounds up out of it: 1,048.570 m is 6 mm from the
        // cell's end on x (half a coarse step is 16 mm).
        let edge = at(Vec3::new(1048.57, 2.0, -5.0));
        assert!(edge.far_pos().is_none() && edge.far_against(&base).is_none());
        // A baseline there has no coarse position: the record against it is absolute.
        let (back, bits) = far_round_trip(&at(Vec3::new(1040.0, 2.0, -5.0)), &edge).expect("far");
        assert_eq!(bits, 1 + FAR_RECORD_BITS as usize);
        assert_eq!(Some(back), at(Vec3::new(1040.0, 2.0, -5.0)).far_shown(&edge));
        // The grid's lowest steps would round below it.
        let low = QTransform {
            pos: QPos::Grid {
                step: DEFAULT_WIRE_STEP_M.to_bits(),
                n: [-MAX_STEPS, 0, 0],
            },
            ..base
        };
        assert!(low.far_pos().is_none());
    }

    /// Hostile far records are errors or valid positions inside the baseline's cell, never a
    /// panic or a position off the grid.
    #[test]
    fn hostile_far_records_stay_in_the_cell() {
        let bases = [
            QTransform::from_net(
                TransformReplication::Full,
                &walker(Vec3::new(1.0, 2.0, 3.0), 0.5, Vec3::X),
                mm(),
            ),
            QTransform::from_net(
                TransformReplication::Movement,
                &walker(Vec3::new(-2_147_000.0, 2_147_000.0, 1048.575), 0.5, Vec3::X),
                mm(),
            ),
            QTransform::from_net(
                TransformReplication::Movement,
                &walker(Vec3::new(1.0, 2.0, 3.0), 0.5, Vec3::X),
                None,
            ),
            QTransform {
                mode: TransformReplication::PositionOnly,
                pos: QPos::Grid {
                    step: DEFAULT_WIRE_STEP_M.to_bits(),
                    n: [-MAX_STEPS, MAX_STEPS, 0],
                },
                rot: None,
                vel: None,
                scale: None,
            },
        ];
        let mut state = 0x0bad_5eedu64;
        let mut decoded = 0u32;
        for len in 0..24usize {
            for _ in 0..256 {
                let bytes: Vec<u8> = (0..len)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        state as u8
                    })
                    .collect();
                for base in &bases {
                    let Ok(q) = QTransform::read_far(base, &mut BitReader::new(&bytes)) else {
                        continue;
                    };
                    decoded += 1;
                    let (QPos::Grid { n, .. }, QPos::Grid { n: bn, .. }) = (q.pos, base.pos) else {
                        panic!("a far record decoded against a raw baseline");
                    };
                    for (a, b) in n.iter().zip(bn) {
                        assert!(a.abs() <= MAX_STEPS);
                        assert_eq!(a.div_euclid(CELL_STEPS), b.div_euclid(CELL_STEPS));
                    }
                    let _ = q.to_net();
                }
            }
        }
        assert!(decoded > 1000, "{decoded}");
    }
}
