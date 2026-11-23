// SPDX-License-Identifier: MPL-2.0
//! [`CharacterTraversal`] on the wire (spec § 8): the codec and the prediction tolerance.

use regia_core::math::Vec3;
use regia_net::bits::{BitReader, BitWriter, DecodeError};
use regia_net::codec::{CodecContext, ReplicateCodec};

use super::{CharacterTraversal, LadderLine, SwimPhase, TraversalKind, TraversalOrder, TraversalRefusal};
use crate::net_character::TOLERANCE_POSITION_M;

const KINDS: [TraversalKind; 8] = [
    TraversalKind::None,
    TraversalKind::Swim,
    TraversalKind::Ladder,
    TraversalKind::LadderEnter,
    TraversalKind::LadderExit,
    TraversalKind::Mantle,
    TraversalKind::Vault,
    TraversalKind::Jump,
];

/// Bits of a refusal on the wire: its index in [`TraversalRefusal::ALL`] plus one, 0 for none.
const REFUSAL_BITS: u32 = 5;

fn kind_tag(kind: TraversalKind) -> u64 {
    KINDS.iter().position(|k| *k == kind).unwrap_or(0) as u64
}

fn write_order(order: Option<&TraversalOrder>, w: &mut BitWriter, cx: &CodecContext) {
    match order {
        None => w.write_bits(0, 2),
        Some(TraversalOrder::Mantle { forward }) => {
            w.write_bits(1, 2);
            forward.encode(w, cx);
        }
        Some(TraversalOrder::Ladder { user_id, near, up }) => {
            w.write_bits(2, 2);
            w.write_varint(*user_id);
            near.encode(w, cx);
            w.write_bool(*up);
        }
        Some(TraversalOrder::Jump { to, arc_height }) => {
            w.write_bits(3, 2);
            to.encode(w, cx);
            w.write_f32(*arc_height);
        }
    }
}

fn read_order(r: &mut BitReader, cx: &CodecContext) -> Result<Option<TraversalOrder>, DecodeError> {
    Ok(match r.read_bits(2)? {
        0 => None,
        1 => Some(TraversalOrder::Mantle {
            forward: Vec3::decode(r, cx)?,
        }),
        2 => Some(TraversalOrder::Ladder {
            user_id: r.read_varint()?,
            near: Vec3::decode(r, cx)?,
            up: r.read_bool()?,
        }),
        _ => Some(TraversalOrder::Jump {
            to: Vec3::decode(r, cx)?,
            arc_height: r.read_f32()?,
        }),
    })
}

/// Whether a state carries a curve's points.
fn carries_path(kind: TraversalKind) -> bool {
    kind.is_curve()
}

/// Whether a state carries a ladder's line.
fn carries_ladder(kind: TraversalKind) -> bool {
    matches!(kind, TraversalKind::Ladder | TraversalKind::LadderEnter)
}

impl ReplicateCodec for CharacterTraversal {
    fn encode(&self, w: &mut BitWriter, cx: &CodecContext) {
        w.write_bits(kind_tag(self.kind), 3);
        w.write_bool(self.phase == SwimPhase::Underwater);
        w.write_f32(self.depth);
        if carries_path(self.kind) {
            w.write_varint(u64::from(self.step));
            w.write_varint(u64::from(self.steps));
            for point in &self.path {
                point.encode(w, cx);
            }
        }
        if carries_ladder(self.kind) {
            self.ladder.line.encode(w, cx);
            self.ladder.into.encode(w, cx);
            w.write_f32(self.ladder.top_y);
            w.write_f32(self.ladder.speed);
            self.ladder.exit.encode(w, cx);
            w.write_varint(self.ladder.id);
        }
        // The frame says what the points mean, and a pending order holds points too.
        for axis in self.frame {
            w.write_bits(axis.to_bits(), 64);
        }
        w.write_bits(u64::from(self.auto_climb.saturating_add(1).clamp(0, 2) as u8), 2);
        w.write_varint(u64::from(self.cooldown_steps));
        match self.last_refusal {
            None => w.write_bits(0, REFUSAL_BITS),
            Some(refusal) => {
                let index = TraversalRefusal::ALL
                    .iter()
                    .position(|r| *r == refusal)
                    .unwrap_or(0);
                w.write_bits(index as u64 + 1, REFUSAL_BITS);
            }
        }
        write_order(self.pending.as_ref(), w, cx);
    }

    /// A state no step could have written (a non-finite point, a step past the end, a line upside
    /// down) is refused here. `step_character` checks again for what reaches it another way.
    fn decode(r: &mut BitReader, cx: &CodecContext) -> Result<Self, DecodeError> {
        let kind = KINDS[r.read_bits(3)? as usize];
        let phase = if r.read_bool()? {
            SwimPhase::Underwater
        } else {
            SwimPhase::Surface
        };
        let depth = r.read_f32()?;
        let mut value = Self {
            kind,
            phase,
            depth,
            ..Self::default()
        };
        if carries_path(kind) {
            value.step =
                u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("traversal step"))?;
            value.steps =
                u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("traversal steps"))?;
            for point in &mut value.path {
                *point = Vec3::decode(r, cx)?;
            }
        }
        if carries_ladder(kind) {
            value.ladder = LadderLine {
                line: Vec3::decode(r, cx)?,
                into: Vec3::decode(r, cx)?,
                top_y: r.read_f32()?,
                speed: r.read_f32()?,
                exit: Vec3::decode(r, cx)?,
                id: r.read_varint()?,
            };
        }
        for axis in &mut value.frame {
            *axis = f64::from_bits(r.read_bits(64)?);
        }
        value.auto_climb = match r.read_bits(2)? {
            0 => -1,
            1 => 0,
            2 => 1,
            _ => return Err(DecodeError::Invalid("traversal auto climb")),
        };
        value.cooldown_steps =
            u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("traversal cooldown"))?;
        value.last_refusal = match r.read_bits(REFUSAL_BITS)? {
            0 => None,
            tag => Some(
                *TraversalRefusal::ALL
                    .get(tag as usize - 1)
                    .ok_or(DecodeError::Invalid("traversal refusal"))?,
            ),
        };
        value.pending = read_order(r, cx)?;
        if !value.is_sane() {
            return Err(DecodeError::Invalid("traversal state"));
        }
        Ok(value)
    }
}

/// The prediction tolerance of [`CharacterTraversal`]: the same move at the same step, its points within
/// [`TOLERANCE_POSITION_M`] in the shared world frame. `last_refusal` counts: both sides decide it from
/// the same state.
pub fn traversal_matches(a: &CharacterTraversal, b: &CharacterTraversal) -> bool {
    let world = |p: Vec3, frame: [f64; 3]| {
        [
            f64::from(p.x) + frame[0],
            f64::from(p.y) + frame[1],
            f64::from(p.z) + frame[2],
        ]
    };
    let near = |x: Vec3, y: Vec3| {
        let (x, y) = (world(x, a.frame), world(y, b.frame));
        let d: f64 = x.iter().zip(y).map(|(p, q)| (p - q) * (p - q)).sum();
        d.sqrt() <= f64::from(TOLERANCE_POSITION_M)
    };
    let in_curve = carries_path(a.kind);
    let on_ladder = carries_ladder(a.kind);
    a.kind == b.kind
        && a.phase == b.phase
        && a.auto_climb == b.auto_climb
        && a.cooldown_steps == b.cooldown_steps
        && a.last_refusal == b.last_refusal
        && a.pending == b.pending
        && (!in_curve
            || (a.step == b.step
                && a.steps == b.steps
                && a.path.iter().zip(&b.path).all(|(p, q)| near(*p, *q))))
        && (!on_ladder || (near(a.ladder.line, b.ladder.line) && a.ladder.id == b.ladder.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_net::codec::encode_to_vec;

    fn round_trip(value: &CharacterTraversal) -> CharacterTraversal {
        let cx = CodecContext::detached();
        let bytes = encode_to_vec(value, &cx);
        let mut r = BitReader::new(&bytes);
        CharacterTraversal::decode(&mut r, &cx).expect("decode")
    }

    #[test]
    fn every_kind_and_refusal_round_trips() {
        for kind in KINDS {
            for refusal in TraversalRefusal::ALL {
                let curve = kind.is_curve();
                let on_ladder = carries_ladder(kind);
                let value = CharacterTraversal {
                    kind,
                    last_refusal: Some(refusal),
                    step: if curve { 7 } else { 0 },
                    steps: if curve { 900 } else { 0 },
                    auto_climb: -1,
                    path: if curve {
                        [Vec3::X, Vec3::Y, Vec3::Z, Vec3::ONE]
                    } else {
                        [Vec3::ZERO; 4]
                    },
                    ladder: if on_ladder {
                        LadderLine {
                            line: Vec3::X,
                            into: Vec3::NEG_Z,
                            top_y: 5.0,
                            speed: 1.5,
                            exit: Vec3::Y,
                            id: 9,
                        }
                    } else {
                        LadderLine::default()
                    },
                    frame: [1024.0, 0.0, -2048.0],
                    ..CharacterTraversal::default()
                };
                assert_eq!(round_trip(&value), value);
            }
        }
    }

    #[test]
    fn every_order_round_trips() {
        for order in [
            TraversalOrder::Mantle { forward: Vec3::X },
            TraversalOrder::Ladder {
                user_id: 1 << 40,
                near: Vec3::Y,
                up: true,
            },
            TraversalOrder::Jump {
                to: Vec3::Z,
                arc_height: 0.5,
            },
        ] {
            let value = CharacterTraversal {
                pending: Some(order),
                ..CharacterTraversal::default()
            };
            assert_eq!(round_trip(&value), value);
        }
    }

    /// A state no step could write must not get past the decoder: each of these panicked or wrote NaN
    /// further down before it was refused here.
    #[test]
    fn a_hostile_state_is_refused_at_the_decoder() {
        let curve = CharacterTraversal {
            kind: TraversalKind::Mantle,
            step: 1,
            steps: 10,
            path: [Vec3::X; 4],
            ..CharacterTraversal::default()
        };
        let on_ladder = CharacterTraversal {
            kind: TraversalKind::Ladder,
            ladder: LadderLine {
                line: Vec3::X,
                into: Vec3::NEG_Z,
                top_y: 5.0,
                speed: 1.5,
                exit: Vec3::Y,
                id: 1,
            },
            ..CharacterTraversal::default()
        };
        assert!(curve.is_sane() && on_ladder.is_sane());
        let mut bad = Vec::new();
        for (i, point) in [f32::NAN, f32::INFINITY, 1.0e30].into_iter().enumerate() {
            let mut value = curve;
            value.path[i + 1].y = point;
            bad.push(value);
        }
        bad.push(CharacterTraversal { step: 11, ..curve });
        bad.push(CharacterTraversal {
            step: u32::MAX,
            steps: u32::MAX,
            ..curve
        });
        let mut value = on_ladder;
        value.ladder.top_y = f32::NAN;
        bad.push(value);
        let mut value = on_ladder;
        value.ladder.top_y = value.ladder.line.y - 1.0;
        bad.push(value);
        let mut value = on_ladder;
        value.ladder.line.x = f32::INFINITY;
        bad.push(value);
        let mut value = on_ladder;
        value.ladder.into = Vec3::ZERO;
        bad.push(value);
        let mut value = curve;
        value.frame[0] = f64::NAN;
        bad.push(value);
        for value in bad {
            let cx = CodecContext::detached();
            let bytes = encode_to_vec(&value, &cx);
            let mut r = BitReader::new(&bytes);
            assert!(CharacterTraversal::decode(&mut r, &cx).is_err(), "{value:?}");
        }
    }

    #[test]
    fn the_tolerance_compares_points_in_the_shared_frame() {
        let mut a = CharacterTraversal {
            kind: TraversalKind::Mantle,
            steps: 10,
            path: [Vec3::new(10.0, 0.0, 0.0); 4],
            ..CharacterTraversal::default()
        };
        let mut b = a;
        // The same world point seen from a peer whose origin is 1024 m further along.
        b.frame = [1024.0, 0.0, 0.0];
        b.path = [Vec3::new(10.0 - 1024.0, 0.0, 0.0); 4];
        assert!(traversal_matches(&a, &b));
        a.path[3].x += 1.0;
        assert!(!traversal_matches(&a, &b));
    }

    #[test]
    fn the_tolerance_allows_small_point_errors_and_nothing_else() {
        let base = CharacterTraversal {
            kind: TraversalKind::Mantle,
            steps: 10,
            path: [Vec3::X; 4],
            ..CharacterTraversal::default()
        };
        let mut near = base;
        near.path[2].x += TOLERANCE_POSITION_M * 0.5;
        assert!(traversal_matches(&base, &near));
        let mut far = base;
        far.path[2].x += TOLERANCE_POSITION_M * 2.0;
        assert!(!traversal_matches(&base, &far));
        let other = CharacterTraversal { step: 1, ..base };
        assert!(!traversal_matches(&base, &other));
        let refused = CharacterTraversal {
            last_refusal: Some(TraversalRefusal::LedgeTooLow),
            ..base
        };
        assert!(!traversal_matches(&base, &refused));
    }
}
