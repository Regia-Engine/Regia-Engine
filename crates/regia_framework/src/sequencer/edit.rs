// SPDX-License-Identifier: MPL-2.0
//! Editing one curve of a sequence: the key edits of the editor's sequence curve panel
//! (`regia_editor::seq_curve_panel`, ADR-1880), as plain data so the panel, its agent methods and
//! tests run the same code.
//!
//! A curve is one component of a float or vec3 track ([`Channel::sample_component`]). An edit
//! changes the channel in place and keeps the keys sorted; a refused edit changes nothing and
//! says why with a lowercase agent error code ([`EditError::code`]). The caller validates the
//! whole sequence afterwards ([`super::Sequence::validate`]).
//!
//! ```
//! use regia_framework::sequencer::edit::{KeyEdit, TangentEdit, apply_key_edit};
//! use regia_framework::sequencer::{Channel, Ease, ScalarKey, Tangent};
//!
//! let mut channel = Channel::Float(vec![ScalarKey::new(0.0, 0.0, Ease::Bezier), ScalarKey::new(1.0, 1.0, Ease::Linear)]);
//! apply_key_edit(&mut channel, 0, KeyEdit::Tangent { index: 0, tangent: TangentEdit::Aligned(2.0) }).expect("edit");
//! let Channel::Float(keys) = &channel else { unreachable!() };
//! assert_eq!(keys[0].tangent, Tangent::Aligned(2.0));
//! ```

use super::asset::{Channel, Ease, ScalarKey, Tangent, Vec3Key};

// Track, marker, key-from-value and binding edits of a whole sequence (ADR-9710).
pub mod tracks;

// Batch key edits across tracks, marker time and payload, shot-list cuts and fades (ADR-7322).
pub mod timeline;

/// A tangent change for one component of one key.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TangentEdit {
    /// [`Tangent::Auto`] (the whole key).
    Auto,
    /// [`Tangent::Flat`] (the whole key).
    Flat,
    /// [`Tangent::Linear`] (the whole key).
    Linear,
    /// One slope on both sides for this component; a vec3 key's other components keep the slopes
    /// they had.
    Aligned(f32),
    /// Separate incoming and outgoing slopes for this component.
    Broken(f32, f32),
}

/// One edit of a curve's keys.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum KeyEdit {
    /// Insert a key at `time`. The component gets `value`; a vec3 key's other components take
    /// the curve's value at `time`, so the other curves keep their shape. The new key eases
    /// [`Ease::Bezier`] with automatic tangents.
    Add {
        /// Seconds on the sequence clock.
        time: f32,
        /// The component's value.
        value: f32,
    },
    /// Move key `index` to `time` and set its component to `value` (ease and tangent kept).
    Move {
        /// Key index.
        index: usize,
        /// Seconds on the sequence clock.
        time: f32,
        /// The component's value.
        value: f32,
    },
    /// Remove key `index` (a curve keeps at least one key).
    Remove {
        /// Key index.
        index: usize,
    },
    /// Change the ease of the segment that starts at key `index`.
    Ease {
        /// Key index.
        index: usize,
        /// The new ease.
        ease: Ease,
    },
    /// Change key `index`'s tangent.
    Tangent {
        /// Key index.
        index: usize,
        /// The new tangent.
        tangent: TangentEdit,
    },
    /// Change key `index`'s handle weights (`None`: unweighted).
    Weights {
        /// Key index.
        index: usize,
        /// `(incoming, outgoing)`, each in `(0, 1]`.
        weights: Option<(f32, f32)>,
    },
}

/// A refused edit: the agent error code and what is wrong. Nothing was changed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct EditError {
    /// Lowercase agent error code (`seq_curve_*`).
    pub code: &'static str,
    /// What is wrong.
    pub message: String,
}

fn refuse<T>(code: &'static str, message: impl Into<String>) -> Result<T, EditError> {
    Err(EditError {
        code,
        message: message.into(),
    })
}

/// Read/write access to one component of a float or vec3 key list.
trait Keys {
    fn len(&self) -> usize;
    fn time(&self, i: usize) -> f32;
    fn set_time_value(&mut self, i: usize, time: f32, c: usize, value: f32);
    fn insert(&mut self, at: usize, time: f32, c: usize, value: f32, others: [f32; 3]);
    fn remove(&mut self, i: usize);
    fn set_ease(&mut self, i: usize, ease: Ease);
    fn set_tangent(&mut self, i: usize, c: usize, tangent: TangentEdit, resolved: [(f32, f32); 3]);
    fn set_weights(&mut self, i: usize, weights: Option<(f32, f32)>);
    fn components(&self) -> usize;
    fn swap(&mut self, a: usize, b: usize);
}

impl Keys for Vec<ScalarKey> {
    fn len(&self) -> usize {
        self.as_slice().len()
    }
    fn time(&self, i: usize) -> f32 {
        self[i].time
    }
    fn set_time_value(&mut self, i: usize, time: f32, _c: usize, value: f32) {
        self[i].time = time;
        self[i].value = value;
    }
    fn insert(&mut self, at: usize, time: f32, _c: usize, value: f32, _others: [f32; 3]) {
        Vec::insert(self, at, ScalarKey::new(time, value, Ease::Bezier));
    }
    fn remove(&mut self, i: usize) {
        Vec::remove(self, i);
    }
    fn set_ease(&mut self, i: usize, ease: Ease) {
        self[i].ease = ease;
    }
    fn set_tangent(&mut self, i: usize, _c: usize, tangent: TangentEdit, _resolved: [(f32, f32); 3]) {
        self[i].tangent = match tangent {
            TangentEdit::Auto => Tangent::Auto,
            TangentEdit::Flat => Tangent::Flat,
            TangentEdit::Linear => Tangent::Linear,
            TangentEdit::Aligned(s) => Tangent::Aligned(s),
            TangentEdit::Broken(a, b) => Tangent::Broken(a, b),
        };
    }
    fn set_weights(&mut self, i: usize, weights: Option<(f32, f32)>) {
        self[i].weights = weights;
    }
    fn components(&self) -> usize {
        1
    }
    fn swap(&mut self, a: usize, b: usize) {
        self.as_mut_slice().swap(a, b);
    }
}

impl Keys for Vec<Vec3Key> {
    fn len(&self) -> usize {
        self.as_slice().len()
    }
    fn time(&self, i: usize) -> f32 {
        self[i].time
    }
    fn set_time_value(&mut self, i: usize, time: f32, c: usize, value: f32) {
        self[i].time = time;
        self[i].value[c] = value;
    }
    fn insert(&mut self, at: usize, time: f32, c: usize, value: f32, mut others: [f32; 3]) {
        others[c] = value;
        Vec::insert(self, at, Vec3Key::new(time, others, Ease::Bezier));
    }
    fn remove(&mut self, i: usize) {
        Vec::remove(self, i);
    }
    fn set_ease(&mut self, i: usize, ease: Ease) {
        self[i].ease = ease;
    }
    fn set_tangent(&mut self, i: usize, c: usize, tangent: TangentEdit, resolved: [(f32, f32); 3]) {
        // A per-component slope keeps the other components where they were drawn.
        let mut incoming = resolved.map(|(a, _)| a);
        let mut outgoing = resolved.map(|(_, b)| b);
        self[i].tangent = match tangent {
            TangentEdit::Auto => Tangent::Auto,
            TangentEdit::Flat => Tangent::Flat,
            TangentEdit::Linear => Tangent::Linear,
            TangentEdit::Aligned(s) => {
                incoming[c] = s;
                outgoing[c] = s;
                if incoming == outgoing {
                    Tangent::Aligned(incoming)
                } else {
                    Tangent::Broken(incoming, outgoing)
                }
            }
            TangentEdit::Broken(a, b) => {
                incoming[c] = a;
                outgoing[c] = b;
                Tangent::Broken(incoming, outgoing)
            }
        };
    }
    fn set_weights(&mut self, i: usize, weights: Option<(f32, f32)>) {
        self[i].weights = weights;
    }
    fn components(&self) -> usize {
        3
    }
    fn swap(&mut self, a: usize, b: usize) {
        self.as_mut_slice().swap(a, b);
    }
}

/// Applies `edit` to component `component` of a float (0) or vec3 (0..3) channel. Returns the
/// index of the key the edit leaves selected (`None` after a removal). Errors (nothing changed):
/// `seq_curve_not_a_curve` (another channel kind), `seq_curve_bad_component`,
/// `seq_curve_bad_key`, `seq_curve_out_of_range` (a time below zero or a non-finite number),
/// `seq_curve_key_time_taken`, `seq_curve_last_key`, `seq_curve_bad_weights`.
pub fn apply_key_edit(
    channel: &mut Channel,
    component: usize,
    edit: KeyEdit,
) -> Result<Option<usize>, EditError> {
    let snapshot = channel.clone();
    let resolved = |i: usize| -> [(f32, f32); 3] {
        let mut out = [(0.0, 0.0); 3];
        for (c, slot) in out.iter_mut().enumerate() {
            *slot = snapshot.tangent_slopes(i, c).unwrap_or((0.0, 0.0));
        }
        out
    };
    let others = |time: f32| -> [f32; 3] {
        let mut out = [0.0; 3];
        for (c, slot) in out.iter_mut().enumerate() {
            *slot = snapshot.sample_component(time, c).unwrap_or(0.0);
        }
        out
    };
    match channel {
        Channel::Float(keys) => edit_keys(keys, component, edit, resolved, others),
        Channel::Vec3(keys) => edit_keys(keys, component, edit, resolved, others),
        other => refuse(
            "seq_curve_not_a_curve",
            format!(
                "a {} track has no curve; only Float and Vec3 tracks do",
                other.kind()
            ),
        ),
    }
}

fn edit_keys(
    keys: &mut impl Keys,
    c: usize,
    edit: KeyEdit,
    resolved: impl Fn(usize) -> [(f32, f32); 3],
    others: impl Fn(f32) -> [f32; 3],
) -> Result<Option<usize>, EditError> {
    if c >= keys.components() {
        return refuse(
            "seq_curve_bad_component",
            format!("component {c}: the track has {}", keys.components()),
        );
    }
    let check_index = |i: usize, len: usize| {
        if i < len {
            Ok(())
        } else {
            refuse(
                "seq_curve_bad_key",
                format!("no key {i}: the curve has {len} keys"),
            )
        }
    };
    let check_point = |time: f32, value: f32| {
        if !(time.is_finite() && time >= 0.0) {
            return refuse(
                "seq_curve_out_of_range",
                format!("time {time} must be zero or more"),
            );
        }
        if !value.is_finite() {
            return refuse("seq_curve_out_of_range", format!("value {value} must be finite"));
        }
        Ok(())
    };
    let taken = |keys: &dyn Fn(usize) -> f32, len: usize, skip: Option<usize>, time: f32| {
        (0..len).any(|i| Some(i) != skip && keys(i) == time)
    };
    match edit {
        KeyEdit::Add { time, value } => {
            check_point(time, value)?;
            if taken(&|i| keys.time(i), keys.len(), None, time) {
                return refuse(
                    "seq_curve_key_time_taken",
                    format!("a key already sits at {time} s"),
                );
            }
            let at = (0..keys.len()).take_while(|&i| keys.time(i) < time).count();
            keys.insert(at, time, c, value, others(time));
            Ok(Some(at))
        }
        KeyEdit::Move { index, time, value } => {
            check_index(index, keys.len())?;
            check_point(time, value)?;
            if taken(&|i| keys.time(i), keys.len(), Some(index), time) {
                return refuse(
                    "seq_curve_key_time_taken",
                    format!("a key already sits at {time} s"),
                );
            }
            keys.set_time_value(index, time, c, value);
            // Bubble the key to its sorted place.
            let mut at = index;
            while at > 0 && keys.time(at - 1) > keys.time(at) {
                keys.swap(at - 1, at);
                at -= 1;
            }
            while at + 1 < keys.len() && keys.time(at + 1) < keys.time(at) {
                keys.swap(at, at + 1);
                at += 1;
            }
            Ok(Some(at))
        }
        KeyEdit::Remove { index } => {
            check_index(index, keys.len())?;
            if keys.len() == 1 {
                return refuse(
                    "seq_curve_last_key",
                    "a curve keeps at least one key; disable the track instead",
                );
            }
            keys.remove(index);
            Ok(None)
        }
        KeyEdit::Ease { index, ease } => {
            check_index(index, keys.len())?;
            keys.set_ease(index, ease);
            Ok(Some(index))
        }
        KeyEdit::Tangent { index, tangent } => {
            check_index(index, keys.len())?;
            let finite = match tangent {
                TangentEdit::Aligned(s) => s.is_finite(),
                TangentEdit::Broken(a, b) => a.is_finite() && b.is_finite(),
                _ => true,
            };
            if !finite {
                return refuse("seq_curve_out_of_range", "tangent slopes must be finite");
            }
            keys.set_tangent(index, c, tangent, resolved(index));
            Ok(Some(index))
        }
        KeyEdit::Weights { index, weights } => {
            check_index(index, keys.len())?;
            if let Some((a, b)) = weights
                && !(a.is_finite() && b.is_finite() && a > 0.0 && b > 0.0 && a <= 1.0 && b <= 1.0)
            {
                return refuse(
                    "seq_curve_bad_weights",
                    format!("weights ({a}, {b}) must each be in (0, 1]"),
                );
            }
            keys.set_weights(index, weights);
            Ok(Some(index))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn float(keys: &[(f32, f32)]) -> Channel {
        Channel::Float(
            keys.iter()
                .map(|&(t, v)| ScalarKey::new(t, v, Ease::Bezier))
                .collect(),
        )
    }

    fn times(channel: &Channel) -> Vec<f32> {
        match channel {
            Channel::Float(k) => k.iter().map(|k| k.time).collect(),
            Channel::Vec3(k) => k.iter().map(|k| k.time).collect(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn adds_moves_and_removes_keep_the_keys_sorted() {
        let mut c = float(&[(0.0, 0.0), (2.0, 1.0)]);
        assert_eq!(
            apply_key_edit(
                &mut c,
                0,
                KeyEdit::Add {
                    time: 1.0,
                    value: 5.0
                }
            ),
            Ok(Some(1))
        );
        assert_eq!(times(&c), [0.0, 1.0, 2.0]);
        assert_eq!(c.sample_component(1.0, 0), Some(5.0));
        // Moving past a neighbor re-sorts and reports the new index.
        assert_eq!(
            apply_key_edit(
                &mut c,
                0,
                KeyEdit::Move {
                    index: 0,
                    time: 3.0,
                    value: 2.0
                }
            ),
            Ok(Some(2))
        );
        assert_eq!(times(&c), [1.0, 2.0, 3.0]);
        assert_eq!(apply_key_edit(&mut c, 0, KeyEdit::Remove { index: 1 }), Ok(None));
        assert_eq!(times(&c), [1.0, 3.0]);
        let code = |r: Result<Option<usize>, EditError>| r.expect_err("refused").code;
        let mut keep = c.clone();
        assert_eq!(
            code(apply_key_edit(
                &mut keep,
                0,
                KeyEdit::Add {
                    time: 1.0,
                    value: 0.0
                }
            )),
            "seq_curve_key_time_taken"
        );
        assert_eq!(
            code(apply_key_edit(
                &mut keep,
                0,
                KeyEdit::Move {
                    index: 0,
                    time: 3.0,
                    value: 0.0
                }
            )),
            "seq_curve_key_time_taken"
        );
        assert_eq!(
            code(apply_key_edit(
                &mut keep,
                0,
                KeyEdit::Add {
                    time: -1.0,
                    value: 0.0
                }
            )),
            "seq_curve_out_of_range"
        );
        assert_eq!(
            code(apply_key_edit(
                &mut keep,
                0,
                KeyEdit::Add {
                    time: 1.5,
                    value: f32::NAN
                }
            )),
            "seq_curve_out_of_range"
        );
        assert_eq!(
            code(apply_key_edit(&mut keep, 0, KeyEdit::Remove { index: 9 })),
            "seq_curve_bad_key"
        );
        assert_eq!(
            code(apply_key_edit(&mut keep, 1, KeyEdit::Remove { index: 0 })),
            "seq_curve_bad_component"
        );
        assert_eq!(keep, c, "refused edits change nothing");
        let mut one = float(&[(0.0, 0.0)]);
        assert_eq!(
            code(apply_key_edit(&mut one, 0, KeyEdit::Remove { index: 0 })),
            "seq_curve_last_key"
        );
        let mut bools = Channel::Bool(vec![]);
        assert_eq!(
            code(apply_key_edit(&mut bools, 0, KeyEdit::Remove { index: 0 })),
            "seq_curve_not_a_curve"
        );
    }

    #[test]
    fn tangent_ease_and_weight_edits_change_the_sampled_curve() {
        let mut c = float(&[(0.0, 0.0), (1.0, 1.0)]);
        let before = c.sample_component(0.25, 0).expect("v");
        apply_key_edit(
            &mut c,
            0,
            KeyEdit::Tangent {
                index: 0,
                tangent: TangentEdit::Aligned(4.0),
            },
        )
        .expect("tangent");
        assert_eq!(c.tangent_slopes(0, 0), Some((4.0, 4.0)));
        assert!(
            c.sample_component(0.25, 0).expect("v") > before,
            "a steeper start"
        );
        apply_key_edit(
            &mut c,
            1,
            KeyEdit::Tangent {
                index: 1,
                tangent: TangentEdit::Broken(-1.0, 3.0),
            },
        )
        .expect_err("component 1 of a float");
        apply_key_edit(
            &mut c,
            0,
            KeyEdit::Tangent {
                index: 1,
                tangent: TangentEdit::Broken(-1.0, 3.0),
            },
        )
        .expect("broken");
        assert_eq!(c.tangent_slopes(1, 0), Some((-1.0, 3.0)));
        apply_key_edit(
            &mut c,
            0,
            KeyEdit::Weights {
                index: 0,
                weights: Some((0.5, 0.8)),
            },
        )
        .expect("weights");
        assert_eq!(
            apply_key_edit(
                &mut c,
                0,
                KeyEdit::Weights {
                    index: 0,
                    weights: Some((0.0, 0.8))
                }
            )
            .expect_err("bad")
            .code,
            "seq_curve_bad_weights"
        );
        apply_key_edit(
            &mut c,
            0,
            KeyEdit::Ease {
                index: 0,
                ease: Ease::Step,
            },
        )
        .expect("ease");
        assert_eq!(c.sample_component(0.5, 0), Some(0.0), "a step holds");
        assert_eq!(
            apply_key_edit(
                &mut c,
                0,
                KeyEdit::Tangent {
                    index: 0,
                    tangent: TangentEdit::Aligned(f32::INFINITY)
                }
            )
            .expect_err("inf")
            .code,
            "seq_curve_out_of_range"
        );
    }

    #[test]
    fn vec3_edits_touch_one_component_and_keep_the_others_where_they_were_drawn() {
        let mut c = Channel::Vec3(vec![
            Vec3Key::new(0.0, [0.0, 10.0, 0.0], Ease::Bezier),
            Vec3Key::new(2.0, [2.0, 10.0, 4.0], Ease::Bezier),
        ]);
        let y_before: Vec<f32> = (0..=8)
            .map(|i| c.sample_component(i as f32 * 0.25, 1).expect("y"))
            .collect();
        // A key added on x takes the other components from the curves at that time.
        let at = apply_key_edit(
            &mut c,
            0,
            KeyEdit::Add {
                time: 1.0,
                value: 7.0,
            },
        )
        .expect("add");
        let Channel::Vec3(keys) = &c else { unreachable!() };
        let mid = &keys[at.expect("index")];
        assert_eq!(mid.value[0], 7.0);
        assert_eq!(mid.value[1], 10.0);
        assert_eq!(mid.value[2], 2.0, "z at 1 s of a flat-ended 0 → 4 curve");
        let y_after: Vec<f32> = (0..=8)
            .map(|i| c.sample_component(i as f32 * 0.25, 1).expect("y"))
            .collect();
        assert_eq!(y_before, y_after, "y did not move");
        // An x slope on an auto key: z keeps its resolved slope.
        let z_slopes = c.tangent_slopes(1, 2).expect("z");
        apply_key_edit(
            &mut c,
            0,
            KeyEdit::Tangent {
                index: 1,
                tangent: TangentEdit::Aligned(-3.0),
            },
        )
        .expect("x slope");
        assert_eq!(c.tangent_slopes(1, 0), Some((-3.0, -3.0)));
        assert_eq!(c.tangent_slopes(1, 2), Some(z_slopes));
        assert_eq!(
            apply_key_edit(&mut c, 3, KeyEdit::Remove { index: 0 })
                .expect_err("component 3")
                .code,
            "seq_curve_bad_component"
        );
    }
}
