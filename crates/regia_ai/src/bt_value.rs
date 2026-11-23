// SPDX-License-Identifier: MPL-2.0
//! Value comparisons for behavior-tree conditions (ADR-4543).
//!
//! A [`BtCompareOp`] relates a blackboard value to a constant or to a second key. The check is
//! total: a missing key, a non-finite float or a value of a different type makes the comparison
//! false. It never panics. Static mismatches (a constant whose type is not the node's declared
//! kind, an ordering the kind does not support) are [`CompareFault`]s the validator reports.

use regia_app::prelude::*;

use crate::blackboard::{BbValue, Blackboard, bb_value_same};

/// How a decorator or condition reacts when one of its blackboard keys changes (ADR-4543).
///
/// `None` keeps the tree's memory: a running branch is resumed and is not interrupted.
/// The other modes preempt that branch, and only when a key the node watches changes.
#[derive(Reflect, Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbortMode {
    /// Do not watch. The running branch is resumed.
    #[default]
    None,
    /// While this node's own branch is running, a failed re-check aborts it.
    #[serde(rename = "self")]
    AbortSelf,
    /// While a lower-priority sibling is running, a passing re-check aborts that sibling.
    LowerPriority,
    /// [`AbortSelf`](Self::AbortSelf) and [`LowerPriority`](Self::LowerPriority).
    Both,
}

/// How a value condition relates the left blackboard value to the right one.
#[derive(Reflect, Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BtCompareOp {
    /// Equal.
    #[default]
    Eq,
    /// Not equal.
    Ne,
    /// Less than.
    Lt,
    /// Less than or equal.
    Le,
    /// Greater than.
    Gt,
    /// Greater than or equal.
    Ge,
    /// The key is present, whatever it holds. The right-hand side is unused.
    IsSet,
    /// Inclusive range. Only integers and floats.
    InRange,
}

/// The type a value condition compares. The blackboard value must be of this type.
#[derive(Reflect, Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BtValueKind {
    /// [`BbValue::Bool`]. `false` is less than `true`.
    #[default]
    Bool,
    /// [`BbValue::Int`].
    Int,
    /// [`BbValue::Float`]. A non-finite float does not compare.
    Float,
    /// [`BbValue::String`], used for tags and enum names. Order is byte lexicographic.
    Tag,
    /// [`BbValue::Entity`], ordered by the entity's raw bits.
    Entity,
}

/// A constant on the right-hand side of a value condition.
#[derive(Reflect, Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BtLiteral {
    /// An integer.
    Int(i64),
    /// A finite float.
    Float(f32),
    /// A boolean.
    Bool(bool),
    /// A tag or enum name.
    Tag(String),
    /// An entity id, as raw bits.
    Entity(u64),
}

impl BtLiteral {
    /// The kind this constant has.
    #[must_use]
    pub fn kind(&self) -> BtValueKind {
        match self {
            Self::Int(_) => BtValueKind::Int,
            Self::Float(_) => BtValueKind::Float,
            Self::Bool(_) => BtValueKind::Bool,
            Self::Tag(_) => BtValueKind::Tag,
            Self::Entity(_) => BtValueKind::Entity,
        }
    }

    /// Whether a float constant is finite. Other kinds are always finite.
    #[must_use]
    pub fn is_finite(&self) -> bool {
        match self {
            Self::Float(v) => v.is_finite(),
            _ => true,
        }
    }
}

/// Why a value condition cannot be compiled. The validator maps these to issue codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareFault {
    /// The left key is empty.
    EmptyKey,
    /// The operator and the declared kind, or a constant and the declared kind, disagree.
    TypeMismatch,
    /// The right-hand side is missing, doubled, non-finite, or out of order.
    BadCompare,
}

/// The fields of one value condition, borrowed from the node that holds them.
#[derive(Clone, Copy)]
pub struct CompareView<'a> {
    /// The relation.
    pub op: BtCompareOp,
    /// The type both sides must have.
    pub kind: BtValueKind,
    /// The left blackboard key.
    pub key: &'a str,
    /// The right blackboard key. Empty means the constant in `literal`.
    pub rhs_key: &'a str,
    /// The constant, when `rhs_key` is empty. The low bound of a range.
    pub literal: &'a Option<BtLiteral>,
    /// The high bound of a range.
    pub literal_hi: &'a Option<BtLiteral>,
}

/// Static problems of `view`, in a stable order. Empty when the node can be compiled.
#[must_use]
pub fn compare_faults(view: CompareView<'_>) -> Vec<CompareFault> {
    let mut faults = Vec::new();
    if view.key.is_empty() {
        faults.push(CompareFault::EmptyKey);
    }
    match view.op {
        BtCompareOp::IsSet => {
            if view.literal.is_some() || view.literal_hi.is_some() || !view.rhs_key.is_empty() {
                faults.push(CompareFault::BadCompare);
            }
        }
        BtCompareOp::InRange => {
            if !matches!(view.kind, BtValueKind::Int | BtValueKind::Float) {
                faults.push(CompareFault::TypeMismatch);
            }
            if !view.rhs_key.is_empty() {
                faults.push(CompareFault::BadCompare);
            }
            match (view.literal, view.literal_hi) {
                (Some(lo), Some(hi)) => {
                    if lo.kind() != view.kind || hi.kind() != view.kind {
                        faults.push(CompareFault::TypeMismatch);
                    }
                    if !lo.is_finite() || !hi.is_finite() || cmp(lo, hi) == std::cmp::Ordering::Greater {
                        faults.push(CompareFault::BadCompare);
                    }
                }
                _ => faults.push(CompareFault::BadCompare),
            }
        }
        BtCompareOp::Eq
        | BtCompareOp::Ne
        | BtCompareOp::Lt
        | BtCompareOp::Le
        | BtCompareOp::Gt
        | BtCompareOp::Ge => {
            let has_key = !view.rhs_key.is_empty();
            let has_lit = view.literal.is_some();
            if has_key == has_lit || view.literal_hi.is_some() {
                faults.push(CompareFault::BadCompare);
            }
            if let Some(lit) = view.literal {
                if lit.kind() != view.kind {
                    faults.push(CompareFault::TypeMismatch);
                }
                if !lit.is_finite() {
                    faults.push(CompareFault::BadCompare);
                }
            }
        }
    }
    faults
}

/// Whether the comparison holds. A missing key or a value of the wrong type is false.
#[must_use]
pub fn holds(bb: &Blackboard, view: CompareView<'_>) -> bool {
    if view.op == BtCompareOp::IsSet {
        return bb.contains(view.key);
    }
    let Some(left) = read_as(bb, view.key, view.kind) else {
        return false;
    };
    if view.op == BtCompareOp::InRange {
        let (Some(lo), Some(hi)) = (view.literal, view.literal_hi) else {
            return false;
        };
        if lo.kind() != view.kind || hi.kind() != view.kind || !lo.is_finite() || !hi.is_finite() {
            return false;
        }
        return cmp(&left, lo) != std::cmp::Ordering::Less && cmp(&left, hi) != std::cmp::Ordering::Greater;
    }
    let right = if !view.rhs_key.is_empty() {
        let Some(value) = read_as(bb, view.rhs_key, view.kind) else {
            return false;
        };
        value
    } else {
        let Some(lit) = view.literal else {
            return false;
        };
        if lit.kind() != view.kind || !lit.is_finite() {
            return false;
        }
        lit.clone()
    };
    apply(view.op, &left, &right)
}

fn read_as(bb: &Blackboard, key: &str, kind: BtValueKind) -> Option<BtLiteral> {
    match (bb.get(key)?, kind) {
        (BbValue::Bool(v), BtValueKind::Bool) => Some(BtLiteral::Bool(*v)),
        (BbValue::Int(v), BtValueKind::Int) => Some(BtLiteral::Int(*v)),
        (BbValue::Float(v), BtValueKind::Float) if v.is_finite() => Some(BtLiteral::Float(*v)),
        (BbValue::String(v), BtValueKind::Tag) => Some(BtLiteral::Tag(v.clone())),
        (BbValue::Entity(v), BtValueKind::Entity) => Some(BtLiteral::Entity(*v)),
        _ => None,
    }
}

fn apply(op: BtCompareOp, left: &BtLiteral, right: &BtLiteral) -> bool {
    let ord = cmp(left, right);
    match op {
        BtCompareOp::Eq => ord == std::cmp::Ordering::Equal,
        BtCompareOp::Ne => ord != std::cmp::Ordering::Equal,
        BtCompareOp::Lt => ord == std::cmp::Ordering::Less,
        BtCompareOp::Le => ord != std::cmp::Ordering::Greater,
        BtCompareOp::Gt => ord == std::cmp::Ordering::Greater,
        BtCompareOp::Ge => ord != std::cmp::Ordering::Less,
        BtCompareOp::IsSet | BtCompareOp::InRange => false,
    }
}

/// Total order. Floats use `<` after non-finite values have been excluded. A pair of different
/// kinds is not ordered; callers have already rejected that pair.
fn cmp(left: &BtLiteral, right: &BtLiteral) -> std::cmp::Ordering {
    match (left, right) {
        (BtLiteral::Int(a), BtLiteral::Int(b)) => a.cmp(b),
        (BtLiteral::Float(a), BtLiteral::Float(b)) => match a.partial_cmp(b) {
            Some(ord) => ord,
            None => std::cmp::Ordering::Equal,
        },
        (BtLiteral::Bool(a), BtLiteral::Bool(b)) => a.cmp(b),
        (BtLiteral::Tag(a), BtLiteral::Tag(b)) => a.cmp(b),
        (BtLiteral::Entity(a), BtLiteral::Entity(b)) => a.cmp(b),
        _ => std::cmp::Ordering::Less,
    }
}

/// Whether two blackboard values are the same for change detection.
///
/// Floats compare by bits, so a NaN does not look changed merely because `NaN != NaN`.
#[must_use]
pub fn watched_same(left: &BbValue, right: &BbValue) -> bool {
    bb_value_same(left, right)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view<'a>(
        op: BtCompareOp,
        kind: BtValueKind,
        key: &'a str,
        rhs_key: &'a str,
        literal: &'a Option<BtLiteral>,
        literal_hi: &'a Option<BtLiteral>,
    ) -> CompareView<'a> {
        CompareView {
            op,
            kind,
            key,
            rhs_key,
            literal,
            literal_hi,
        }
    }

    fn lit(kind: BtValueKind, n: i64) -> BtLiteral {
        match kind {
            BtValueKind::Bool => BtLiteral::Bool(n != 0),
            BtValueKind::Int => BtLiteral::Int(n),
            BtValueKind::Float => BtLiteral::Float(n as f32),
            BtValueKind::Tag => BtLiteral::Tag(if n <= 0 { "m" } else { "z" }.into()),
            BtValueKind::Entity => BtLiteral::Entity(n as u64),
        }
    }

    fn put(bb: &mut Blackboard, kind: BtValueKind, n: i64) {
        match kind {
            BtValueKind::Bool => bb.insert("k", n != 0),
            BtValueKind::Int => bb.insert("k", n),
            BtValueKind::Float => bb.insert("k", n as f32),
            BtValueKind::Tag => bb.insert("k", if n <= 0 { "m" } else { "z" }),
            BtValueKind::Entity => bb.insert("k", BbValue::Entity(n as u64)),
        };
    }

    fn check(op: BtCompareOp, kind: BtValueKind, stored: i64, against: i64) -> bool {
        let literal = Some(lit(kind, against));
        let mut bb = Blackboard::new();
        put(&mut bb, kind, stored);
        holds(&bb, view(op, kind, "k", "", &literal, &None))
    }

    #[test]
    fn every_operator_has_a_true_and_a_false_case_and_a_type_mismatch_is_not_a_panic() {
        // 0 is the low sample and 1 the high sample for every kind: false < true, "m" < "z".
        let table = [
            (BtCompareOp::Eq, true, false),
            (BtCompareOp::Ne, false, true),
            (BtCompareOp::Lt, false, true),
            (BtCompareOp::Le, true, true),
            (BtCompareOp::Gt, false, false),
            (BtCompareOp::Ge, true, false),
        ];
        let kinds = [
            BtValueKind::Bool,
            BtValueKind::Int,
            BtValueKind::Float,
            BtValueKind::Tag,
            BtValueKind::Entity,
        ];
        for (op, same, low_vs_high) in table {
            for kind in kinds {
                let stored = Some(lit(kind, 0));
                assert!(
                    compare_faults(view(op, kind, "k", "", &stored, &None)).is_empty(),
                    "{op:?} {kind:?}"
                );
                let wrong_kind = if kind == BtValueKind::Int {
                    BtValueKind::Float
                } else {
                    BtValueKind::Int
                };
                let wrong = Some(lit(wrong_kind, 1));
                assert!(
                    compare_faults(view(op, kind, "k", "", &wrong, &None))
                        .contains(&CompareFault::TypeMismatch),
                    "{op:?} {kind:?} against {wrong_kind:?}"
                );
                assert_eq!(check(op, kind, 0, 0), same, "equal samples {op:?} {kind:?}");
                assert_eq!(
                    check(op, kind, 0, 1),
                    low_vs_high,
                    "low against high {op:?} {kind:?}"
                );
                // The opposite order, so Lt and Gt each have a true case and a false case.
                let high_vs_low = match op {
                    BtCompareOp::Eq => false,
                    BtCompareOp::Ne => true,
                    BtCompareOp::Lt => false,
                    BtCompareOp::Le => false,
                    BtCompareOp::Gt => true,
                    BtCompareOp::Ge => true,
                    BtCompareOp::IsSet | BtCompareOp::InRange => false,
                };
                assert_eq!(
                    check(op, kind, 1, 0),
                    high_vs_low,
                    "high against low {op:?} {kind:?}"
                );
                let mut bb = Blackboard::new();
                bb.insert("k", BbValue::Vec3(regia_core::math::Vec3::ZERO));
                assert!(
                    !holds(&bb, view(op, kind, "k", "", &stored, &None)),
                    "a type mismatch is false, not a panic ({op:?} {kind:?})"
                );
            }
        }
    }

    #[test]
    fn is_set_and_in_range_cover_each_kind() {
        for kind in [
            BtValueKind::Bool,
            BtValueKind::Int,
            BtValueKind::Float,
            BtValueKind::Tag,
            BtValueKind::Entity,
        ] {
            let none = None;
            assert!(compare_faults(view(BtCompareOp::IsSet, kind, "k", "", &none, &none)).is_empty());
            let extra = Some(lit(kind, 1));
            assert!(
                compare_faults(view(BtCompareOp::IsSet, kind, "k", "", &extra, &none))
                    .contains(&CompareFault::BadCompare)
            );
            let mut bb = Blackboard::new();
            assert!(!holds(&bb, view(BtCompareOp::IsSet, kind, "k", "", &none, &none)));
            put(&mut bb, kind, 1);
            assert!(holds(&bb, view(BtCompareOp::IsSet, kind, "k", "", &none, &none)));
            // A present value of another type still counts as set.
            bb.insert("k", BbValue::Vec3(regia_core::math::Vec3::ZERO));
            assert!(holds(&bb, view(BtCompareOp::IsSet, kind, "k", "", &none, &none)));

            let lo = Some(lit(kind, 0));
            let hi = Some(lit(kind, 2));
            let faults = compare_faults(view(BtCompareOp::InRange, kind, "k", "", &lo, &hi));
            if matches!(kind, BtValueKind::Int | BtValueKind::Float) {
                assert!(faults.is_empty(), "{kind:?} {faults:?}");
                let mut bb = Blackboard::new();
                put(&mut bb, kind, 1);
                assert!(holds(&bb, view(BtCompareOp::InRange, kind, "k", "", &lo, &hi)));
                put(&mut bb, kind, 3);
                assert!(!holds(&bb, view(BtCompareOp::InRange, kind, "k", "", &lo, &hi)));
                put(&mut bb, kind, 0);
                assert!(
                    holds(&bb, view(BtCompareOp::InRange, kind, "k", "", &lo, &hi)),
                    "inclusive"
                );
                bb.insert("k", true);
                assert!(!holds(&bb, view(BtCompareOp::InRange, kind, "k", "", &lo, &hi)));
            } else {
                assert!(faults.contains(&CompareFault::TypeMismatch), "{kind:?}");
            }
        }
        let nan = Some(BtLiteral::Float(f32::NAN));
        let one = Some(BtLiteral::Float(1.0));
        assert!(
            compare_faults(view(
                BtCompareOp::InRange,
                BtValueKind::Float,
                "k",
                "",
                &nan,
                &one
            ))
            .contains(&CompareFault::BadCompare)
        );
        let hi = Some(BtLiteral::Int(1));
        let lo = Some(BtLiteral::Int(3));
        assert!(
            compare_faults(view(BtCompareOp::InRange, BtValueKind::Int, "k", "", &lo, &hi))
                .contains(&CompareFault::BadCompare)
        );
    }

    #[test]
    fn a_second_key_compares_and_a_type_mismatch_is_false() {
        let none = None;
        let mut bb = Blackboard::new();
        bb.insert("left", 2_i64);
        bb.insert("right", 2_i64);
        assert!(holds(
            &bb,
            view(BtCompareOp::Eq, BtValueKind::Int, "left", "right", &none, &none)
        ));
        bb.insert("right", 3_i64);
        assert!(!holds(
            &bb,
            view(BtCompareOp::Eq, BtValueKind::Int, "left", "right", &none, &none)
        ));
        bb.insert("right", 1.5_f32);
        assert!(!holds(
            &bb,
            view(BtCompareOp::Lt, BtValueKind::Int, "left", "right", &none, &none)
        ));
        let both = Some(BtLiteral::Int(1));
        assert!(
            compare_faults(view(
                BtCompareOp::Eq,
                BtValueKind::Int,
                "left",
                "right",
                &both,
                &none
            ))
            .contains(&CompareFault::BadCompare)
        );
    }
}
