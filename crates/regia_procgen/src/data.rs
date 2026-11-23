// SPDX-License-Identifier: MPL-2.0
//! The data that flows over graph wires (spec 62 § 3): attribute values and columns, point sets,
//! tables, items with tags and item scope, and bundles.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use regia_core::math::Vec3;

use crate::rot::Rot;

/// How many times a shared [`Column`] was copied so a write could go through (process-wide).
static COLUMN_COPIES: AtomicU64 = AtomicU64::new(0);
/// The bytes those copies moved.
static COLUMN_COPY_BYTES: AtomicU64 = AtomicU64::new(0);

/// `(copies, bytes)` of shared [`Column`]s copied by writes so far in this process: the clone
/// counter of copy-on-write point data (ADR-2731). A linear chain of in-place nodes without a cache
/// leaves it unchanged. Process-wide, so compare two readings of a test that runs alone.
pub fn column_copy_stats() -> (u64, u64) {
    (
        COLUMN_COPIES.load(Ordering::Relaxed),
        COLUMN_COPY_BYTES.load(Ordering::Relaxed),
    )
}

/// A copy-on-write column: `clone` shares the elements, the first write to a shared column copies
/// them (ADR-2731). It reads as the `Vec` it holds. A write through [`std::ops::DerefMut`] pays an
/// atomic uniqueness check each time, so a loop over many elements takes [`Column::make_mut`] once.
#[derive(Debug)]
pub struct Column<T>(Arc<Vec<T>>);

impl<T: Clone> Column<T> {
    /// The elements for writing, copied first when another column shares them.
    pub fn make_mut(&mut self) -> &mut Vec<T> {
        if Arc::strong_count(&self.0) > 1 || Arc::weak_count(&self.0) > 0 {
            COLUMN_COPIES.fetch_add(1, Ordering::Relaxed);
            COLUMN_COPY_BYTES.fetch_add(
                (self.0.len() * std::mem::size_of::<T>()) as u64,
                Ordering::Relaxed,
            );
        }
        Arc::make_mut(&mut self.0)
    }

    /// The elements as a `Vec`, without a copy when this column is the only holder.
    pub fn into_vec(self) -> Vec<T> {
        Arc::try_unwrap(self.0).unwrap_or_else(|shared| (*shared).clone())
    }
}

impl<T> Column<T> {
    /// Whether both columns share one allocation (a copy-on-write check for tests and tools).
    pub fn shares_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl<T> Clone for Column<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T> Default for Column<T> {
    fn default() -> Self {
        Self(Arc::new(Vec::new()))
    }
}

impl<T> std::ops::Deref for Column<T> {
    type Target = Vec<T>;

    fn deref(&self) -> &Vec<T> {
        &self.0
    }
}

impl<T: Clone> std::ops::DerefMut for Column<T> {
    fn deref_mut(&mut self) -> &mut Vec<T> {
        self.make_mut()
    }
}

impl<T> From<Vec<T>> for Column<T> {
    fn from(v: Vec<T>) -> Self {
        Self(Arc::new(v))
    }
}

impl<T: Clone> From<Column<T>> for Vec<T> {
    fn from(c: Column<T>) -> Self {
        c.into_vec()
    }
}

impl<T> FromIterator<T> for Column<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self(Arc::new(iter.into_iter().collect()))
    }
}

impl<T: Clone> IntoIterator for Column<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.into_vec().into_iter()
    }
}

impl<'a, T> IntoIterator for &'a Column<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<'a, T: Clone> IntoIterator for &'a mut Column<T> {
    type Item = &'a mut T;
    type IntoIter = std::slice::IterMut<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.make_mut().iter_mut()
    }
}

impl<T: Clone> Extend<T> for Column<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        self.make_mut().extend(iter);
    }
}

// Element by element, never by pointer: a NaN compares unequal to itself, and a shared allocation
// must answer what two separate copies of it would.
impl<T: PartialEq> PartialEq for Column<T> {
    fn eq(&self, other: &Self) -> bool {
        *self.0 == *other.0
    }
}

impl<T: PartialEq> PartialEq<Vec<T>> for Column<T> {
    fn eq(&self, other: &Vec<T>) -> bool {
        *self.0 == *other
    }
}

impl<T: PartialEq> PartialEq<Column<T>> for Vec<T> {
    fn eq(&self, other: &Column<T>) -> bool {
        *self == *other.0
    }
}

impl<T: PartialEq, const N: usize> PartialEq<[T; N]> for Column<T> {
    fn eq(&self, other: &[T; N]) -> bool {
        self.0.as_slice() == other.as_slice()
    }
}

impl<T: PartialEq> PartialEq<[T]> for Column<T> {
    fn eq(&self, other: &[T]) -> bool {
        self.0.as_slice() == other
    }
}

/// Value types of attributes (spec 62 § 3.1). The order is the conversion priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AttrType {
    /// `bool`.
    Bool,
    /// `i64`.
    Int,
    /// `f64`.
    Float,
    /// Two f64 components.
    Vec2,
    /// Three f64 components.
    Vec3,
    /// Four f64 components.
    Vec4,
    /// A unit quaternion `[x, y, z, w]` in f64.
    Quat,
    /// Translation, rotation, scale.
    Transform,
    /// A UTF-8 name (≤ 256 bytes).
    Name,
    /// A project-relative asset path or `guid:<32 hex>`.
    Asset,
}

impl AttrType {
    /// Every type, in priority order.
    pub const ALL: [AttrType; 10] = [
        Self::Bool,
        Self::Int,
        Self::Float,
        Self::Vec2,
        Self::Vec3,
        Self::Vec4,
        Self::Quat,
        Self::Transform,
        Self::Name,
        Self::Asset,
    ];

    /// The name used in files and by the agent (`"Float"`, ...).
    pub fn name(self) -> &'static str {
        match self {
            Self::Bool => "Bool",
            Self::Int => "Int",
            Self::Float => "Float",
            Self::Vec2 => "Vec2",
            Self::Vec3 => "Vec3",
            Self::Vec4 => "Vec4",
            Self::Quat => "Quat",
            Self::Transform => "Transform",
            Self::Name => "Name",
            Self::Asset => "Asset",
        }
    }

    /// Parses [`AttrType::name`].
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.name() == name)
    }

    /// Number of f64 components of a numeric type (`None` for names, assets and transforms).
    pub fn width(self) -> Option<usize> {
        match self {
            Self::Bool | Self::Int | Self::Float => Some(1),
            Self::Vec2 => Some(2),
            Self::Vec3 => Some(3),
            Self::Vec4 | Self::Quat => Some(4),
            Self::Transform | Self::Name | Self::Asset => None,
        }
    }
}

/// A transform value in f64 (attribute math precision).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Xform {
    /// Translation.
    pub t: [f64; 3],
    /// Rotation quaternion `[x, y, z, w]`.
    pub r: [f64; 4],
    /// Scale.
    pub s: [f64; 3],
}

impl Default for Xform {
    fn default() -> Self {
        Self {
            t: [0.0; 3],
            r: [0.0, 0.0, 0.0, 1.0],
            s: [1.0; 3],
        }
    }
}

/// One attribute value.
#[derive(Clone, Debug, PartialEq)]
pub enum AttrValue {
    /// A boolean.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A float.
    Float(f64),
    /// A 2-vector.
    Vec2([f64; 2]),
    /// A 3-vector.
    Vec3([f64; 3]),
    /// A 4-vector.
    Vec4([f64; 4]),
    /// A quaternion.
    Quat([f64; 4]),
    /// A transform.
    Transform(Xform),
    /// A name.
    Name(String),
    /// An asset reference.
    Asset(String),
}

impl AttrValue {
    /// The value's type.
    pub fn ty(&self) -> AttrType {
        match self {
            Self::Bool(_) => AttrType::Bool,
            Self::Int(_) => AttrType::Int,
            Self::Float(_) => AttrType::Float,
            Self::Vec2(_) => AttrType::Vec2,
            Self::Vec3(_) => AttrType::Vec3,
            Self::Vec4(_) => AttrType::Vec4,
            Self::Quat(_) => AttrType::Quat,
            Self::Transform(_) => AttrType::Transform,
            Self::Name(_) => AttrType::Name,
            Self::Asset(_) => AttrType::Asset,
        }
    }

    /// The numeric components (bool → 0/1, int widened); `None` for names, assets, transforms.
    pub fn components(&self) -> Option<Vec<f64>> {
        Some(match self {
            Self::Bool(b) => vec![f64::from(u8::from(*b))],
            Self::Int(i) => vec![*i as f64],
            Self::Float(f) => vec![*f],
            Self::Vec2(v) => v.to_vec(),
            Self::Vec3(v) => v.to_vec(),
            Self::Vec4(v) | Self::Quat(v) => v.to_vec(),
            Self::Transform(_) | Self::Name(_) | Self::Asset(_) => return None,
        })
    }

    /// Text form of names and assets, and of ints (for key matching, spec 63 § 13.1).
    pub fn as_key_text(&self) -> Option<String> {
        match self {
            Self::Name(s) | Self::Asset(s) => Some(s.clone()),
            Self::Int(i) => Some(i.to_string()),
            Self::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }

    /// The key every keyed node uses (`attr.lookup`, `items.partition`, `spawn.*` `ByAttr` and
    /// `GroupThenWeighted`, `flow.match`/`flow.pick`): [`AttrValue::as_key_text`], else the
    /// components as a list (`[1.0]`, `[1.0, 2.0]`), so a float key groups and matches the same way
    /// in every node (audit M8: spawn and flow used to see `""`).
    pub fn key_text(&self) -> String {
        self.as_key_text()
            .or_else(|| self.components().map(|x| format!("{x:?}")))
            .unwrap_or_default()
    }
}

/// One typed column (custom attributes of points, columns of tables).
#[derive(Clone, Debug, PartialEq)]
pub enum AttrColumn {
    /// Booleans.
    Bool(Column<bool>),
    /// Integers.
    Int(Column<i64>),
    /// Floats.
    Float(Column<f64>),
    /// 2-vectors.
    Vec2(Column<[f64; 2]>),
    /// 3-vectors.
    Vec3(Column<[f64; 3]>),
    /// 4-vectors.
    Vec4(Column<[f64; 4]>),
    /// Quaternions.
    Quat(Column<[f64; 4]>),
    /// Transforms.
    Transform(Column<Xform>),
    /// Names.
    Name(Column<String>),
    /// Asset references.
    Asset(Column<String>),
}

impl AttrColumn {
    /// An empty column of `ty`.
    pub fn new(ty: AttrType) -> Self {
        Self::filled(ty, 0)
    }

    /// A column of `len` default values (0, false, empty, identity).
    pub fn filled(ty: AttrType, len: usize) -> Self {
        match ty {
            AttrType::Bool => Self::Bool(vec![false; len].into()),
            AttrType::Int => Self::Int(vec![0; len].into()),
            AttrType::Float => Self::Float(vec![0.0; len].into()),
            AttrType::Vec2 => Self::Vec2(vec![[0.0; 2]; len].into()),
            AttrType::Vec3 => Self::Vec3(vec![[0.0; 3]; len].into()),
            AttrType::Vec4 => Self::Vec4(vec![[0.0; 4]; len].into()),
            AttrType::Quat => Self::Quat(vec![[0.0, 0.0, 0.0, 1.0]; len].into()),
            AttrType::Transform => Self::Transform(vec![Xform::default(); len].into()),
            AttrType::Name => Self::Name(vec![String::new(); len].into()),
            AttrType::Asset => Self::Asset(vec![String::new(); len].into()),
        }
    }

    /// The column's type.
    pub fn ty(&self) -> AttrType {
        match self {
            Self::Bool(_) => AttrType::Bool,
            Self::Int(_) => AttrType::Int,
            Self::Float(_) => AttrType::Float,
            Self::Vec2(_) => AttrType::Vec2,
            Self::Vec3(_) => AttrType::Vec3,
            Self::Vec4(_) => AttrType::Vec4,
            Self::Quat(_) => AttrType::Quat,
            Self::Transform(_) => AttrType::Transform,
            Self::Name(_) => AttrType::Name,
            Self::Asset(_) => AttrType::Asset,
        }
    }

    /// Number of elements.
    pub fn len(&self) -> usize {
        match self {
            Self::Bool(v) => v.len(),
            Self::Int(v) => v.len(),
            Self::Float(v) => v.len(),
            Self::Vec2(v) => v.len(),
            Self::Vec3(v) => v.len(),
            Self::Vec4(v) | Self::Quat(v) => v.len(),
            Self::Transform(v) => v.len(),
            Self::Name(v) | Self::Asset(v) => v.len(),
        }
    }

    /// Whether the column has no elements.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Element `i` as a value.
    pub fn get(&self, i: usize) -> Option<AttrValue> {
        Some(match self {
            Self::Bool(v) => AttrValue::Bool(*v.get(i)?),
            Self::Int(v) => AttrValue::Int(*v.get(i)?),
            Self::Float(v) => AttrValue::Float(*v.get(i)?),
            Self::Vec2(v) => AttrValue::Vec2(*v.get(i)?),
            Self::Vec3(v) => AttrValue::Vec3(*v.get(i)?),
            Self::Vec4(v) => AttrValue::Vec4(*v.get(i)?),
            Self::Quat(v) => AttrValue::Quat(*v.get(i)?),
            Self::Transform(v) => AttrValue::Transform(*v.get(i)?),
            Self::Name(v) => AttrValue::Name(v.get(i)?.clone()),
            Self::Asset(v) => AttrValue::Asset(v.get(i)?.clone()),
        })
    }

    /// Appends a value; `false` (and nothing appended) when the type differs.
    pub fn push(&mut self, value: AttrValue) -> bool {
        match (self, value) {
            (Self::Bool(v), AttrValue::Bool(x)) => v.push(x),
            (Self::Int(v), AttrValue::Int(x)) => v.push(x),
            (Self::Float(v), AttrValue::Float(x)) => v.push(x),
            (Self::Vec2(v), AttrValue::Vec2(x)) => v.push(x),
            (Self::Vec3(v), AttrValue::Vec3(x)) => v.push(x),
            (Self::Vec4(v), AttrValue::Vec4(x)) => v.push(x),
            (Self::Quat(v), AttrValue::Quat(x)) => v.push(x),
            (Self::Transform(v), AttrValue::Transform(x)) => v.push(x),
            (Self::Name(v), AttrValue::Name(x)) => v.push(x),
            (Self::Asset(v), AttrValue::Asset(x)) => v.push(x),
            _ => return false,
        }
        true
    }

    /// The elements at `indices`, in that order.
    pub fn select(&self, indices: &[usize]) -> Self {
        fn pick<T: Clone>(v: &[T], idx: &[usize]) -> Column<T> {
            idx.iter().map(|&i| v[i].clone()).collect()
        }
        match self {
            Self::Bool(v) => Self::Bool(pick(v, indices)),
            Self::Int(v) => Self::Int(pick(v, indices)),
            Self::Float(v) => Self::Float(pick(v, indices)),
            Self::Vec2(v) => Self::Vec2(pick(v, indices)),
            Self::Vec3(v) => Self::Vec3(pick(v, indices)),
            Self::Vec4(v) => Self::Vec4(pick(v, indices)),
            Self::Quat(v) => Self::Quat(pick(v, indices)),
            Self::Transform(v) => Self::Transform(pick(v, indices)),
            Self::Name(v) => Self::Name(pick(v, indices)),
            Self::Asset(v) => Self::Asset(pick(v, indices)),
        }
    }

    /// Appends `other` (same type) or `n` defaults when `other` is `None`.
    pub fn extend_from(&mut self, other: Option<&AttrColumn>, n: usize) -> bool {
        let filler = match other {
            Some(o) if o.ty() == self.ty() => o.clone(),
            Some(_) => return false,
            None => AttrColumn::filled(self.ty(), n),
        };
        match (self, filler) {
            (Self::Bool(a), Self::Bool(b)) => a.extend(b),
            (Self::Int(a), Self::Int(b)) => a.extend(b),
            (Self::Float(a), Self::Float(b)) => a.extend(b),
            (Self::Vec2(a), Self::Vec2(b)) => a.extend(b),
            (Self::Vec3(a), Self::Vec3(b)) => a.extend(b),
            (Self::Vec4(a), Self::Vec4(b)) => a.extend(b),
            (Self::Quat(a), Self::Quat(b)) => a.extend(b),
            (Self::Transform(a), Self::Transform(b)) => a.extend(b),
            (Self::Name(a), Self::Name(b)) => a.extend(b),
            (Self::Asset(a), Self::Asset(b)) => a.extend(b),
            _ => return false,
        }
        true
    }

    /// Overwrites element `dst` with element `src_i` of `src`.
    ///
    /// Returns false when the column types differ or either index is past the end.
    /// Numeric values are assigned. Names and assets clone the string.
    pub(crate) fn copy_at(&mut self, dst: usize, src: &Self, src_i: usize) -> bool {
        if self.ty() != src.ty() || dst >= self.len() || src_i >= src.len() {
            return false;
        }
        match (self, src) {
            (Self::Bool(a), Self::Bool(b)) => a.make_mut()[dst] = b[src_i],
            (Self::Int(a), Self::Int(b)) => a.make_mut()[dst] = b[src_i],
            (Self::Float(a), Self::Float(b)) => a.make_mut()[dst] = b[src_i],
            (Self::Vec2(a), Self::Vec2(b)) => a.make_mut()[dst] = b[src_i],
            (Self::Vec3(a), Self::Vec3(b)) => a.make_mut()[dst] = b[src_i],
            (Self::Vec4(a), Self::Vec4(b)) => a.make_mut()[dst] = b[src_i],
            (Self::Quat(a), Self::Quat(b)) => a.make_mut()[dst] = b[src_i],
            (Self::Transform(a), Self::Transform(b)) => a.make_mut()[dst] = b[src_i],
            (Self::Name(a), Self::Name(b)) => a.make_mut()[dst].clone_from(&b[src_i]),
            (Self::Asset(a), Self::Asset(b)) => a.make_mut()[dst].clone_from(&b[src_i]),
            _ => return false,
        }
        true
    }

    /// Element `i` as f64 components (numeric types only).
    pub fn components(&self, i: usize) -> Option<Vec<f64>> {
        self.get(i)?.components()
    }

    /// The first non-finite element index, if any (float-carrying columns only).
    pub fn first_non_finite(&self) -> Option<usize> {
        fn find<const N: usize>(v: &[[f64; N]]) -> Option<usize> {
            v.iter().position(|x| x.iter().any(|c| !c.is_finite()))
        }
        match self {
            Self::Float(v) => v.iter().position(|x| !x.is_finite()),
            Self::Vec2(v) => find(v),
            Self::Vec3(v) => find(v),
            Self::Vec4(v) | Self::Quat(v) => find(v),
            Self::Transform(v) => v.iter().position(|x| {
                x.t.iter()
                    .chain(x.r.iter())
                    .chain(x.s.iter())
                    .any(|c| !c.is_finite())
            }),
            _ => None,
        }
    }

    fn hash_into(&self, h: &mut blake3::Hasher) {
        h.update(self.ty().name().as_bytes());
        h.update(&(self.len() as u64).to_le_bytes());
        let f = |h: &mut blake3::Hasher, x: f64| {
            h.update(&x.to_bits().to_le_bytes());
        };
        match self {
            Self::Bool(v) => v.iter().for_each(|x| {
                h.update(&[u8::from(*x)]);
            }),
            Self::Int(v) => v.iter().for_each(|x| {
                h.update(&x.to_le_bytes());
            }),
            Self::Float(v) => v.iter().for_each(|x| f(h, *x)),
            Self::Vec2(v) => v.iter().flatten().for_each(|x| f(h, *x)),
            Self::Vec3(v) => v.iter().flatten().for_each(|x| f(h, *x)),
            Self::Vec4(v) | Self::Quat(v) => v.iter().flatten().for_each(|x| f(h, *x)),
            Self::Transform(v) => v.iter().for_each(|x| {
                x.t.iter()
                    .chain(x.r.iter())
                    .chain(x.s.iter())
                    .for_each(|c| f(h, *c));
            }),
            Self::Name(v) | Self::Asset(v) => v.iter().for_each(|s| {
                h.update(&(s.len() as u64).to_le_bytes());
                h.update(s.as_bytes());
            }),
        }
    }
}

/// Names a custom attribute may not take (spec 62 § 3.2).
pub const BUILTIN_NAMES: [&str; 11] = [
    "position",
    "rotation",
    "scale",
    "color",
    "density",
    "seed",
    "bounds_min",
    "bounds_max",
    "hardness",
    "index",
    "count",
];

/// Whether `name` is reserved (a built-in property, `index`, `count`, or starts with `_`).
pub fn is_reserved_name(name: &str) -> bool {
    name.starts_with('_') || BUILTIN_NAMES.contains(&name)
}

/// One point with its built-in properties, for building sets.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    /// World position, meters.
    pub position: [f32; 3],
    /// Rotation quaternion `[x, y, z, w]`.
    pub rotation: [f32; 4],
    /// Per-axis scale.
    pub scale: [f32; 3],
    /// Linear RGBA tint.
    pub color: [f32; 4],
    /// Existence likelihood in [0, 1].
    pub density: f32,
    /// Random stream key.
    pub seed: u32,
    /// Local box minimum before scale.
    pub bounds_min: [f32; 3],
    /// Local box maximum before scale.
    pub bounds_max: [f32; 3],
    /// Box edge sharpness in [0, 1].
    pub hardness: f32,
}

impl Default for Point {
    fn default() -> Self {
        Self {
            position: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            color: [1.0; 4],
            density: 1.0,
            seed: 0,
            bounds_min: [-0.5; 3],
            bounds_max: [0.5; 3],
            hardness: 1.0,
        }
    }
}

/// [`PointSet`]'s columns held for writing ([`PointSet::writer`]).
pub struct PointWriter<'a> {
    /// World positions.
    pub position: &'a mut Vec<[f32; 3]>,
    /// Rotations `[x, y, z, w]`.
    pub rotation: &'a mut Vec<[f32; 4]>,
    /// Scales.
    pub scale: &'a mut Vec<[f32; 3]>,
    /// Linear RGBA tints.
    pub color: &'a mut Vec<[f32; 4]>,
    /// Densities in [0, 1].
    pub density: &'a mut Vec<f32>,
    /// Seeds.
    pub seed: &'a mut Vec<u32>,
    /// Local box minima.
    pub bounds_min: &'a mut Vec<[f32; 3]>,
    /// Local box maxima.
    pub bounds_max: &'a mut Vec<[f32; 3]>,
    /// Hardness in [0, 1].
    pub hardness: &'a mut Vec<f32>,
    /// Custom attributes, each as long as the built-in columns.
    pub attrs: &'a mut BTreeMap<String, AttrColumn>,
}

impl PointWriter<'_> {
    /// Appends one point; custom attributes get their type's default.
    pub fn push(&mut self, p: Point) {
        self.position.push(p.position);
        self.rotation.push(p.rotation);
        self.scale.push(p.scale);
        self.color.push(p.color);
        self.density.push(p.density.clamp(0.0, 1.0));
        self.seed.push(p.seed);
        self.bounds_min.push(p.bounds_min);
        self.bounds_max.push(p.bounds_max);
        self.hardness.push(p.hardness.clamp(0.0, 1.0));
        for col in self.attrs.values_mut() {
            col.extend_from(None, 1);
        }
    }

    /// Overwrites point `i`'s built-in properties (density and hardness clamped).
    pub fn set_point(&mut self, i: usize, p: Point) {
        self.position[i] = p.position;
        self.rotation[i] = p.rotation;
        self.scale[i] = p.scale;
        self.color[i] = p.color;
        self.density[i] = p.density.clamp(0.0, 1.0);
        self.seed[i] = p.seed;
        self.bounds_min[i] = p.bounds_min;
        self.bounds_max[i] = p.bounds_max;
        self.hardness[i] = p.hardness.clamp(0.0, 1.0);
    }
}

/// A set of points: built-in properties as parallel columns plus named custom attributes
/// (spec 62 § 3.2).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PointSet {
    /// World positions.
    pub position: Column<[f32; 3]>,
    /// Rotations `[x, y, z, w]`.
    pub rotation: Column<[f32; 4]>,
    /// Scales.
    pub scale: Column<[f32; 3]>,
    /// Linear RGBA tints.
    pub color: Column<[f32; 4]>,
    /// Densities in [0, 1].
    pub density: Column<f32>,
    /// Seeds.
    pub seed: Column<u32>,
    /// Local box minima.
    pub bounds_min: Column<[f32; 3]>,
    /// Local box maxima.
    pub bounds_max: Column<[f32; 3]>,
    /// Hardness in [0, 1].
    pub hardness: Column<f32>,
    /// Custom attributes, each `len()` long.
    pub attrs: BTreeMap<String, AttrColumn>,
}

impl PointSet {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of points.
    pub fn len(&self) -> usize {
        self.position.len()
    }

    /// Whether the set has no points.
    pub fn is_empty(&self) -> bool {
        self.position.is_empty()
    }

    /// Appends one point; custom attributes get their type's default. A loop over many points takes
    /// [`PointSet::writer`] once instead (each column's copy-on-write check is an atomic operation).
    pub fn push(&mut self, p: Point) {
        self.writer().push(p);
    }

    /// The columns for writing, each copied first when shared (ADR-2731).
    pub fn writer(&mut self) -> PointWriter<'_> {
        PointWriter {
            position: self.position.make_mut(),
            rotation: self.rotation.make_mut(),
            scale: self.scale.make_mut(),
            color: self.color.make_mut(),
            density: self.density.make_mut(),
            seed: self.seed.make_mut(),
            bounds_min: self.bounds_min.make_mut(),
            bounds_max: self.bounds_max.make_mut(),
            hardness: self.hardness.make_mut(),
            attrs: &mut self.attrs,
        }
    }

    /// Point `i`'s built-in properties.
    pub fn point(&self, i: usize) -> Point {
        Point {
            position: self.position[i],
            rotation: self.rotation[i],
            scale: self.scale[i],
            color: self.color[i],
            density: self.density[i],
            seed: self.seed[i],
            bounds_min: self.bounds_min[i],
            bounds_max: self.bounds_max[i],
            hardness: self.hardness[i],
        }
    }

    /// Overwrites point `i`'s built-in properties (density and hardness clamped). A loop over many
    /// points takes [`PointSet::writer`] once instead.
    pub fn set_point(&mut self, i: usize, p: Point) {
        self.writer().set_point(i, p);
    }

    /// The points at `indices`, in that order, with all attributes.
    pub fn select(&self, indices: &[usize]) -> Self {
        fn pick<T: Copy>(v: &[T], idx: &[usize]) -> Column<T> {
            idx.iter().map(|&i| v[i]).collect()
        }
        Self {
            position: pick(&self.position, indices),
            rotation: pick(&self.rotation, indices),
            scale: pick(&self.scale, indices),
            color: pick(&self.color, indices),
            density: pick(&self.density, indices),
            seed: pick(&self.seed, indices),
            bounds_min: pick(&self.bounds_min, indices),
            bounds_max: pick(&self.bounds_max, indices),
            hardness: pick(&self.hardness, indices),
            attrs: self
                .attrs
                .iter()
                .map(|(k, c)| (k.clone(), c.select(indices)))
                .collect(),
        }
    }

    /// Splits into (kept, rejected) by `keep[i]`, keeping order.
    pub fn partition(&self, keep: &[bool]) -> (Self, Self) {
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for (i, k) in keep.iter().enumerate() {
            if *k {
                a.push(i);
            } else {
                b.push(i);
            }
        }
        (self.select(&a), self.select(&b))
    }

    /// Appends all points of `other`. Attributes missing on either side are filled with defaults.
    ///
    /// # Errors
    ///
    /// The attribute name whose type differs between the two sets (`attr_type_conflict`).
    pub fn append(&mut self, other: &PointSet) -> Result<(), String> {
        for (name, col) in &other.attrs {
            if let Some(mine) = self.attrs.get(name)
                && mine.ty() != col.ty()
            {
                return Err(name.clone());
            }
        }
        let (n0, n1) = (self.len(), other.len());
        for (name, col) in &other.attrs {
            self.attrs
                .entry(name.clone())
                .or_insert_with(|| AttrColumn::filled(col.ty(), n0));
        }
        for (name, col) in &mut self.attrs {
            col.extend_from(other.attrs.get(name), n1);
        }
        self.position.extend_from_slice(&other.position);
        self.rotation.extend_from_slice(&other.rotation);
        self.scale.extend_from_slice(&other.scale);
        self.color.extend_from_slice(&other.color);
        self.density.extend_from_slice(&other.density);
        self.seed.extend_from_slice(&other.seed);
        self.bounds_min.extend_from_slice(&other.bounds_min);
        self.bounds_max.extend_from_slice(&other.bounds_max);
        self.hardness.extend_from_slice(&other.hardness);
        Ok(())
    }

    /// Point `i`'s world-space axis-aligned box `(min, max)` of its oriented box.
    pub fn world_aabb(&self, i: usize) -> ([f32; 3], [f32; 3]) {
        let q = quat(self.rotation[i]);
        let s = Vec3::from(self.scale[i]);
        let lo = Vec3::from(self.bounds_min[i]) * s;
        let hi = Vec3::from(self.bounds_max[i]) * s;
        let center = q * ((lo + hi) * 0.5);
        let half = (hi - lo).abs() * 0.5;
        // Extent of a rotated box: |R| * half.
        let m = q.to_mat3();
        let ext = Vec3::new(
            m.x_axis.x.abs() * half.x + m.y_axis.x.abs() * half.y + m.z_axis.x.abs() * half.z,
            m.x_axis.y.abs() * half.x + m.y_axis.y.abs() * half.y + m.z_axis.y.abs() * half.z,
            m.x_axis.z.abs() * half.x + m.y_axis.z.abs() * half.y + m.z_axis.z.abs() * half.z,
        );
        let c = Vec3::from(self.position[i]) + center;
        ((c - ext).to_array(), (c + ext).to_array())
    }

    /// The region value of point `i` at world point `p` (spec 63 § 2): `density · edge`, where
    /// `edge` is 1 inside the box shrunk by `hardness` and falls linearly to 0 at the box surface.
    pub fn contains(&self, i: usize, p: [f32; 3]) -> f32 {
        let q = quat(self.rotation[i]);
        let s = Vec3::from(self.scale[i]);
        let local = q.inverse() * (Vec3::from(p) - Vec3::from(self.position[i]));
        let lo = Vec3::from(self.bounds_min[i]) * s;
        let hi = Vec3::from(self.bounds_max[i]) * s;
        let (lo, hi) = (lo.min(hi), lo.max(hi));
        if local.cmplt(lo).any() || local.cmpgt(hi).any() {
            return 0.0;
        }
        let hardness = self.hardness[i];
        let edge = if hardness >= 1.0 {
            1.0
        } else {
            // Normalized distance from the center per axis: 0 at center, 1 on the surface.
            let center = (lo + hi) * 0.5;
            let half = ((hi - lo) * 0.5).max(Vec3::splat(1e-6));
            let d = ((local - center).abs() / half).max_element().min(1.0);
            let inner = hardness;
            if d <= inner {
                1.0
            } else {
                ((1.0 - d) / (1.0 - inner)).clamp(0.0, 1.0)
            }
        };
        self.density[i] * edge
    }

    /// Checks every float property and attribute for NaN/infinity: `(attribute, first index)`.
    pub fn first_non_finite(&self) -> Option<(String, usize)> {
        fn arr<const N: usize>(v: &[[f32; N]]) -> Option<usize> {
            v.iter().position(|x| x.iter().any(|c| !c.is_finite()))
        }
        let checks: [(&str, Option<usize>); 8] = [
            ("position", arr(&self.position)),
            ("rotation", arr(&self.rotation)),
            ("scale", arr(&self.scale)),
            ("color", arr(&self.color)),
            ("density", self.density.iter().position(|x| !x.is_finite())),
            ("bounds_min", arr(&self.bounds_min)),
            ("bounds_max", arr(&self.bounds_max)),
            ("hardness", self.hardness.iter().position(|x| !x.is_finite())),
        ];
        for (name, idx) in checks {
            if let Some(i) = idx {
                return Some((name.to_string(), i));
            }
        }
        self.attrs
            .iter()
            .find_map(|(k, c)| c.first_non_finite().map(|i| (k.clone(), i)))
    }

    fn hash_into(&self, h: &mut blake3::Hasher) {
        h.update(b"points");
        h.update(&(self.len() as u64).to_le_bytes());
        let f = |h: &mut blake3::Hasher, x: &f32| {
            h.update(&x.to_bits().to_le_bytes());
        };
        self.position.iter().flatten().for_each(|x| f(h, x));
        self.rotation.iter().flatten().for_each(|x| f(h, x));
        self.scale.iter().flatten().for_each(|x| f(h, x));
        self.color.iter().flatten().for_each(|x| f(h, x));
        self.density.iter().for_each(|x| f(h, x));
        self.seed.iter().for_each(|x| {
            h.update(&x.to_le_bytes());
        });
        self.bounds_min.iter().flatten().for_each(|x| f(h, x));
        self.bounds_max.iter().flatten().for_each(|x| f(h, x));
        self.hardness.iter().for_each(|x| f(h, x));
        for (name, col) in &self.attrs {
            h.update(name.as_bytes());
            col.hash_into(h);
        }
    }
}

/// A `[x, y, z, w]` array as a normalized quaternion (identity when degenerate).
pub fn quat(r: [f32; 4]) -> Rot {
    let q = Rot(r);
    let len = q.length();
    if len.is_finite() && len > 1e-12 {
        q.normalize()
    } else {
        Rot::IDENTITY
    }
}

/// Rows of named typed columns with no spatial meaning (spec 62 § 3.3).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Table {
    /// Number of rows.
    pub rows: usize,
    /// Columns in insertion order, each `rows` long.
    pub columns: Vec<(String, AttrColumn)>,
}

impl Table {
    /// The first non-finite cell as `(column, row)`. The run's `attr_nan` gate reads point sets, so a
    /// table captured from a file is checked where it enters the run (spec 62 § 5.2, `input_nan`).
    pub fn first_non_finite(&self) -> Option<(&str, usize)> {
        fn first<const N: usize>(v: &[[f64; N]]) -> Option<usize> {
            v.iter().position(|c| c.iter().any(|x| !x.is_finite()))
        }
        self.columns.iter().find_map(|(name, column)| {
            let row = match column {
                AttrColumn::Float(v) => v.iter().position(|x| !x.is_finite()),
                AttrColumn::Vec2(v) => first(v),
                AttrColumn::Vec3(v) => first(v),
                AttrColumn::Vec4(v) | AttrColumn::Quat(v) => first(v),
                _ => None,
            };
            row.map(|r| (name.as_str(), r))
        })
    }

    /// A one-row table from `(name, value)` pairs.
    pub fn one_row(values: impl IntoIterator<Item = (String, AttrValue)>) -> Self {
        let mut t = Table {
            rows: 1,
            columns: Vec::new(),
        };
        for (name, v) in values {
            let mut col = AttrColumn::new(v.ty());
            col.push(v);
            t.columns.push((name, col));
        }
        t
    }

    /// The column named `name`.
    pub fn column(&self, name: &str) -> Option<&AttrColumn> {
        self.columns.iter().find(|(k, _)| k == name).map(|(_, c)| c)
    }

    /// Sets (or adds) a column; its length must equal `rows`.
    pub fn set_column(&mut self, name: &str, col: AttrColumn) {
        if let Some(slot) = self.columns.iter_mut().find(|(k, _)| k == name) {
            slot.1 = col;
        } else {
            self.columns.push((name.to_string(), col));
        }
    }

    /// The rows at `indices`, in that order.
    pub fn select(&self, indices: &[usize]) -> Self {
        Self {
            rows: indices.len(),
            columns: self
                .columns
                .iter()
                .map(|(k, c)| (k.clone(), c.select(indices)))
                .collect(),
        }
    }

    fn hash_into(&self, h: &mut blake3::Hasher) {
        h.update(b"table");
        h.update(&(self.rows as u64).to_le_bytes());
        for (name, col) in &self.columns {
            h.update(name.as_bytes());
            col.hash_into(h);
        }
    }
}

/// Reads a data table file (`.table.ron`, `data_table` v1 `(rows: { "key": (field: value, ...) })`, or v0's bare
/// `[(key, row)]` list) **untyped** into a [`Table`] (spec 63 § 3 `in.table`): a `key` column (Name) plus one column per
/// row field, typed per column: numbers → Float (Int when every value is integral), booleans → Bool, strings and enum
/// variants → Name, tuples of 2-4 numbers → vectors. Rows keep file order.
///
/// # Errors
///
/// `table_parse: ...` for a malformed file, `table_column_types: ...` for a column mixing types.
pub fn table_from_ron(text: &str) -> Result<Table, String> {
    use crate::value::SettingValue as V;
    let root = V::parse(text).map_err(|e| format!("table_parse: {e}"))?;
    let rows: Vec<(String, Vec<(String, V)>)> = match &root {
        V::Record(_, fields) => match fields.iter().find(|(k, _)| k == "rows").map(|(_, v)| v) {
            Some(V::Map(entries)) => entries
                .iter()
                .map(|(k, v)| match v {
                    V::Record(_, f) => Ok((k.as_text().unwrap_or("").to_string(), f.clone())),
                    _ => Err("table_parse: rows are records".to_string()),
                })
                .collect::<Result<_, _>>()?,
            _ => return Err("table_parse: missing `rows: { ... }`".to_string()),
        },
        V::List(items) => items
            .iter()
            .map(|it| match it.as_items() {
                Some([k, V::Record(_, f)]) => Ok((k.as_text().unwrap_or("").to_string(), f.clone())),
                _ => Err("table_parse: v0 rows are (key, (field: value))".to_string()),
            })
            .collect::<Result<_, _>>()?,
        _ => return Err("table_parse: not a data table".to_string()),
    };
    let mut names: Vec<String> = Vec::new();
    for (_, fields) in &rows {
        for (k, _) in fields {
            if !names.contains(k) {
                names.push(k.clone());
            }
        }
    }
    let mut t = Table {
        rows: rows.len(),
        columns: vec![(
            "key".to_string(),
            AttrColumn::Name(rows.iter().map(|(k, _)| k.clone()).collect()),
        )],
    };
    for name in names {
        let cells: Vec<Option<&V>> = rows
            .iter()
            .map(|(_, f)| f.iter().find(|(k, _)| *k == name).map(|(_, v)| v))
            .collect();
        let present: Vec<&V> = cells.iter().flatten().copied().collect();
        let vec_width = |v: &V| {
            v.as_items()
                .filter(|i| (2..=4).contains(&i.len()) && i.iter().all(|x| x.as_f64().is_some()))
                .map(<[V]>::len)
        };
        let col = if present.iter().all(|v| matches!(v, V::Int(_))) {
            AttrColumn::Int(
                cells
                    .iter()
                    .map(|c| c.and_then(|v| v.as_i64()).unwrap_or(0))
                    .collect(),
            )
        } else if present.iter().all(|v| matches!(v, V::Int(_) | V::Float(_))) {
            AttrColumn::Float(
                cells
                    .iter()
                    .map(|c| c.and_then(|v| v.as_f64()).unwrap_or(0.0))
                    .collect(),
            )
        } else if present.iter().all(|v| matches!(v, V::Bool(_))) {
            AttrColumn::Bool(
                cells
                    .iter()
                    .map(|c| c.and_then(|v| v.as_bool()).unwrap_or(false))
                    .collect(),
            )
        } else if present.iter().all(|v| matches!(v, V::Str(_) | V::Ident(_))) {
            AttrColumn::Name(
                cells
                    .iter()
                    .map(|c| c.and_then(|v| v.as_text()).unwrap_or("").to_string())
                    .collect(),
            )
        } else if let Some(w) = present
            .first()
            .and_then(|v| vec_width(v))
            .filter(|w| present.iter().all(|v| vec_width(v) == Some(*w)))
        {
            let comps = |c: &Option<&V>| -> Vec<f64> {
                c.and_then(|v| v.as_items())
                    .map(|i| i.iter().filter_map(V::as_f64).collect())
                    .unwrap_or_else(|| vec![0.0; w])
            };
            match w {
                2 => AttrColumn::Vec2(
                    cells
                        .iter()
                        .map(|c| {
                            let v = comps(c);
                            [v[0], v[1]]
                        })
                        .collect(),
                ),
                3 => AttrColumn::Vec3(
                    cells
                        .iter()
                        .map(|c| {
                            let v = comps(c);
                            [v[0], v[1], v[2]]
                        })
                        .collect(),
                ),
                _ => AttrColumn::Vec4(
                    cells
                        .iter()
                        .map(|c| {
                            let v = comps(c);
                            [v[0], v[1], v[2], v[3]]
                        })
                        .collect(),
                ),
            }
        } else {
            return Err(format!("table_column_types: column `{name}` mixes value types"));
        };
        t.columns.push((name, col));
    }
    Ok(t)
}

/// The kinds of payload a pin may carry (spec 62 § 3.3); a bit set in [`KindSet`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DataKind {
    /// [`PointSet`].
    Points,
    /// [`Table`].
    Table,
    /// Spline data (spec 64).
    Spline,
    /// Polygon data (spec 64).
    Polygon,
    /// [`ShapeData`].
    Shape,
    /// [`GroundData`].
    Ground,
    /// A 2D scalar image over an XZ rectangle (spec 63 § 3).
    Mask,
    /// A mesh reference (spec 63 § 4).
    Mesh,
    /// Grown plant skeletons (spec 68, plant domain).
    PlantSkeleton,
    /// Meshed plants (spec 68, plant domain).
    PlantMesh,
}

impl DataKind {
    /// Every kind.
    pub const ALL: [DataKind; 10] = [
        Self::Points,
        Self::Table,
        Self::Spline,
        Self::Polygon,
        Self::Shape,
        Self::Ground,
        Self::Mask,
        Self::Mesh,
        Self::PlantSkeleton,
        Self::PlantMesh,
    ];

    /// The name used in graph files (`"Points"`, ...).
    pub fn name(self) -> &'static str {
        match self {
            Self::Points => "Points",
            Self::Table => "Table",
            Self::Spline => "Spline",
            Self::Polygon => "Polygon",
            Self::Shape => "Shape",
            Self::Ground => "Ground",
            Self::Mask => "Mask",
            Self::Mesh => "Mesh",
            Self::PlantSkeleton => "PlantSkeleton",
            Self::PlantMesh => "PlantMesh",
        }
    }

    /// Parses [`DataKind::name`].
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }

    fn bit(self) -> u16 {
        1 << (self as u16)
    }
}

/// A set of [`DataKind`]s.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct KindSet(pub u16);

impl KindSet {
    /// No kinds.
    pub const NONE: KindSet = KindSet(0);
    /// Every kind.
    pub const ANY: KindSet = KindSet(0x3ff);
    /// Points only.
    pub const POINTS: KindSet = KindSet(1);
    /// Tables only.
    pub const TABLE: KindSet = KindSet(2);
    /// The spatial kinds (spec 62 § 3.3): points, spline, polygon, shape, ground, mask.
    pub const SPATIAL: KindSet = KindSet(1 | 4 | 8 | 16 | 32 | 64);

    /// A set of the listed kinds.
    pub fn of(kinds: &[DataKind]) -> Self {
        Self(kinds.iter().fold(0, |a, k| a | k.bit()))
    }

    /// Whether `kind` is in the set.
    pub fn contains(self, kind: DataKind) -> bool {
        self.0 & kind.bit() != 0
    }

    /// Whether the two sets share a kind.
    pub fn intersects(self, other: KindSet) -> bool {
        self.0 & other.0 != 0
    }

    /// The kinds, in [`DataKind::ALL`] order.
    pub fn kinds(self) -> Vec<DataKind> {
        DataKind::ALL.into_iter().filter(|k| self.contains(*k)).collect()
    }

    /// The kind names.
    pub fn names(self) -> Vec<&'static str> {
        self.kinds().into_iter().map(DataKind::name).collect()
    }
}

/// A terrain capture: heights over a sample-aligned sub-grid of the terrain (spec 63 § 3).
///
/// Sampling matches `regia_terrain::TerrainMap::sample_bilinear` and `normal_at` exactly for every
/// query whose stencil lies inside the captured grid; the host captures the run rectangle plus a
/// two-sample margin.
#[derive(Clone, Debug, PartialEq)]
pub struct GroundData {
    /// The rectangle this item stands for, `[min_x, min_z, max_x, max_z]`.
    pub rect: [f32; 4],
    /// Full-terrain sample columns and rows (for the world → grid mapping).
    pub map_size: [u32; 2],
    /// World width of the full terrain, meters.
    pub world_width: f32,
    /// First captured column and row of the full grid.
    pub origin: [u32; 2],
    /// Captured columns and rows.
    pub size: [u32; 2],
    /// Captured heights, row-major.
    pub heights: Vec<f32>,
    /// The terrain layers the graph's `in.ground` nodes name, by the settings' order; empty when
    /// none is named (spec 63 § 3).
    pub layers: Vec<GroundLayer>,
}

/// One terrain layer's weights over a [`GroundData`] grid.
#[derive(Clone, Debug, PartialEq)]
pub struct GroundLayer {
    /// The layer's name in the terrain's layer stack.
    pub name: String,
    /// Weights in [0, 1], row-major, one per captured height.
    pub weights: Vec<f32>,
}

impl GroundData {
    /// Captures `rect` (plus `margin` samples) from a terrain map.
    ///
    /// The rectangle is ordered to `[min_x, min_z, max_x, max_z]` before the columns are counted.
    /// An inverted rect used to wrap the `u32` column count and then panic in [`Self::height`].
    pub fn capture(map: &regia_terrain::TerrainMap, rect: [f32; 4], margin: u32) -> Self {
        let rect = [
            rect[0].min(rect[2]),
            rect[1].min(rect[3]),
            rect[0].max(rect[2]),
            rect[1].max(rect[3]),
        ];
        let (w, h) = (map.width(), map.height());
        if w == 0 || h == 0 {
            return Self {
                rect,
                map_size: [w, h],
                world_width: map.world_width(),
                origin: [0, 0],
                size: [0, 0],
                heights: Vec::new(),
                layers: Vec::new(),
            };
        }
        let [c0, r0] = map.world_to_grid(rect[0], rect[1]);
        let [c1, r1] = map.world_to_grid(rect[2], rect[3]);
        let clampc = |v: f32, n: u32| -> u32 {
            if v.is_nan() {
                0
            } else {
                v.clamp(0.0, (n - 1) as f32) as u32
            }
        };
        let mut col0 = clampc(c0.floor(), w).saturating_sub(margin);
        let mut row0 = clampc(r0.floor(), h).saturating_sub(margin);
        let mut col1 = (clampc(c1.ceil(), w) + margin).min(w - 1);
        let mut row1 = (clampc(r1.ceil(), h) + margin).min(h - 1);
        // A reversed grid (a negative map width) still counts upward.
        if col0 > col1 {
            std::mem::swap(&mut col0, &mut col1);
        }
        if row0 > row1 {
            std::mem::swap(&mut row0, &mut row1);
        }
        let cols = col1 - col0 + 1;
        let rows = row1 - row0 + 1;
        let mut heights = Vec::with_capacity((cols * rows) as usize);
        for r in row0..=row1 {
            for c in col0..=col1 {
                heights.push(map.height_at_sample(i64::from(c), i64::from(r)));
            }
        }
        Self {
            rect,
            map_size: [w, h],
            world_width: map.world_width(),
            origin: [col0, row0],
            size: [cols, rows],
            heights,
            layers: Vec::new(),
        }
    }

    /// Captures the layers `names` asks for: `weight(name, slope radians, altitude, [x, z])` for each
    /// captured sample (slope through [`crate::detmath`]). A name `weight` answers `None` for at the
    /// first sample is left out; `in.ground` reports it (`layer_missing`).
    pub fn capture_layers(
        &mut self,
        names: impl IntoIterator<Item = String>,
        weight: impl Fn(&str, f32, f32, [f32; 2]) -> Option<f32>,
    ) {
        let half = self.world_width * 0.5;
        let step = |n: u32| self.world_width / n.saturating_sub(1).max(1) as f32;
        let (sx, sz) = (step(self.map_size[0]), step(self.map_size[1]));
        let cols = self.size[0].max(1) as usize;
        let at = |i: usize| {
            let (c, r) = (
                (i % cols) as u32 + self.origin[0],
                (i / cols) as u32 + self.origin[1],
            );
            [c as f32 * sx - half, r as f32 * sz - half]
        };
        let mut layers = Vec::new();
        for name in names {
            let values: Option<Vec<f32>> = self
                .heights
                .iter()
                .enumerate()
                .map(|(i, &y)| {
                    let [x, z] = at(i);
                    let n = self.normal(x, z);
                    let slope = crate::detmath::acos(f64::from(n[1]).clamp(-1.0, 1.0)) as f32;
                    weight(&name, slope, y, [x, z]).map(|w| w.clamp(0.0, 1.0))
                })
                .collect();
            if let Some(weights) = values {
                layers.push(GroundLayer { name, weights });
            }
        }
        self.layers = layers;
    }

    /// The bilinear weight of the captured layer `name` at world XZ (`None` when not captured).
    pub fn layer_weight(&self, name: &str, x: f32, z: f32) -> Option<f32> {
        let layer = self.layers.iter().find(|l| l.name == name)?;
        Some(self.bilinear(&layer.weights, x, z))
    }

    fn grid(&self, x: f32, z: f32) -> [f32; 2] {
        let half = self.world_width * 0.5;
        [
            (x + half) / self.world_width * (self.map_size[0] - 1) as f32,
            (z + half) / self.world_width * (self.map_size[1] - 1) as f32,
        ]
    }

    fn sample(&self, values: &[f32], col: i64, row: i64) -> f32 {
        // `clamp` panics when the upper bound is below the lower one, which a zero size is.
        if self.size[0] == 0 || self.size[1] == 0 {
            return 0.0;
        }
        let c = (col - i64::from(self.origin[0])).clamp(0, i64::from(self.size[0]) - 1) as usize;
        let r = (row - i64::from(self.origin[1])).clamp(0, i64::from(self.size[1]) - 1) as usize;
        values[r * self.size[0] as usize + c]
    }

    /// Bilinear height at world XZ (the formula of `TerrainMap::sample_bilinear_grid`).
    ///
    /// An empty capture (a zero column or row count, or a map with no samples) returns 0.
    pub fn height(&self, x: f32, z: f32) -> f32 {
        self.bilinear(&self.heights, x, z)
    }

    /// `values` (one per captured sample) at world XZ, bilinear.
    fn bilinear(&self, values: &[f32], x: f32, z: f32) -> f32 {
        // A zero map size underflows `map_size - 1` before `sample` can refuse the empty grid.
        if self.size[0] == 0
            || self.size[1] == 0
            || self.map_size[0] == 0
            || self.map_size[1] == 0
            || values.is_empty()
        {
            return 0.0;
        }
        let [u, v] = self.grid(x, z);
        let (w, h) = (self.map_size[0] as usize, self.map_size[1] as usize);
        let u = if u.is_nan() { 0.0 } else { u };
        let v = if v.is_nan() { 0.0 } else { v };
        let u = u.clamp(0.0, (w - 1) as f32);
        let v = v.clamp(0.0, (h - 1) as f32);
        let iu = u.floor() as usize;
        let iv = v.floor() as usize;
        let iu1 = (iu + 1).min(w - 1);
        let iv1 = (iv + 1).min(h - 1);
        let fu = u - iu as f32;
        let fv = v - iv as f32;
        let s = |c: usize, r: usize| self.sample(values, c as i64, r as i64);
        let a = s(iu, iv) + (s(iu1, iv) - s(iu, iv)) * fu;
        let b = s(iu, iv1) + (s(iu1, iv1) - s(iu, iv1)) * fu;
        a + (b - a) * fv
    }

    /// Surface normal at world XZ (the formula of `TerrainMap::normal_at`).
    pub fn normal(&self, x: f32, z: f32) -> [f32; 3] {
        if self.map_size[0] == 0 || self.map_size[1] == 0 {
            return [0.0, 1.0, 0.0];
        }
        let sx = self.world_width / (self.map_size[0] - 1) as f32;
        let sz = self.world_width / (self.map_size[1] - 1) as f32;
        let h_l = self.height(x - sx, z);
        let h_r = self.height(x + sx, z);
        let h_d = self.height(x, z - sz);
        let h_u = self.height(x, z + sz);
        let n = [(h_l - h_r) * sz, 2.0 * sx * sz, (h_d - h_u) * sx];
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        if len.is_finite() && len > 0.0 {
            [n[0] / len, n[1] / len, n[2] / len]
        } else {
            [0.0, 1.0, 0.0]
        }
    }

    fn hash_into(&self, h: &mut blake3::Hasher) {
        h.update(b"ground");
        for v in self.rect {
            h.update(&v.to_bits().to_le_bytes());
        }
        for v in self
            .map_size
            .iter()
            .chain(self.origin.iter())
            .chain(self.size.iter())
        {
            h.update(&v.to_le_bytes());
        }
        h.update(&self.world_width.to_bits().to_le_bytes());
        for v in &self.heights {
            h.update(&v.to_bits().to_le_bytes());
        }
        // Only captured layers enter the hash, so a capture without them keeps its old hash.
        for layer in &self.layers {
            h.update(b"layer");
            h.update(layer.name.as_bytes());
            for v in &layer.weights {
                h.update(&v.to_bits().to_le_bytes());
            }
        }
    }
}

/// The geometric form of a [`ShapeData`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ShapeForm {
    /// A box with these half extents.
    Box {
        /// Half extents, meters.
        half_extents: [f32; 3],
    },
    /// A sphere.
    Sphere {
        /// Radius, meters.
        radius: f32,
    },
}

/// A region with no points: a shape with a transform and an edge band (spec 62 § 3.3).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShapeData {
    /// The form.
    pub form: ShapeForm,
    /// World position of the shape's center.
    pub position: [f32; 3],
    /// World rotation `[x, y, z, w]`.
    pub rotation: [f32; 4],
    /// Linear falloff width outside the shape, meters (0 = hard edge).
    pub edge_m: f32,
    /// Tiled runs (spec 66 § 2): the region is cut to this XZ rectangle `[min_x, min_z, max_x,
    /// max_z]` (value 0 outside, AABB clipped); `None` everywhere else.
    pub clip_xz: Option<[f32; 4]>,
}

impl ShapeData {
    /// The region value at `p` (spec 63 § 2): 1 inside, linear falloff over `edge_m` outside.
    pub fn value(&self, p: [f32; 3]) -> f32 {
        if let Some(c) = self.clip_xz
            && (p[0] < c[0] || p[0] > c[2] || p[2] < c[1] || p[2] > c[3])
        {
            return 0.0;
        }
        let local = quat(self.rotation).inverse() * (Vec3::from(p) - Vec3::from(self.position));
        let outside = match self.form {
            ShapeForm::Box { half_extents } => {
                // A negative half extent is 0, the same rule as `PreparedShape`.
                let d = local.abs() - Vec3::from(half_extents).max(Vec3::ZERO);
                d.max(Vec3::ZERO).length()
            }
            ShapeForm::Sphere { radius } => (local.length() - radius.max(0.0)).max(0.0),
        };
        if outside <= 0.0 {
            1.0
        } else if self.edge_m > 0.0 {
            (1.0 - outside / self.edge_m).max(0.0)
        } else {
            0.0
        }
    }

    /// World-space AABB `(min, max)` including the edge band.
    pub fn world_aabb(&self) -> ([f32; 3], [f32; 3]) {
        let ext = match self.form {
            ShapeForm::Box { half_extents } => {
                let m = quat(self.rotation).to_mat3();
                let h = Vec3::from(half_extents).max(Vec3::ZERO);
                Vec3::new(
                    m.x_axis.x.abs() * h.x + m.y_axis.x.abs() * h.y + m.z_axis.x.abs() * h.z,
                    m.x_axis.y.abs() * h.x + m.y_axis.y.abs() * h.y + m.z_axis.y.abs() * h.z,
                    m.x_axis.z.abs() * h.x + m.y_axis.z.abs() * h.y + m.z_axis.z.abs() * h.z,
                )
            }
            ShapeForm::Sphere { radius } => Vec3::splat(radius.max(0.0)),
        } + Vec3::splat(self.edge_m.max(0.0));
        let c = Vec3::from(self.position);
        let (mut lo, mut hi) = ((c - ext).to_array(), (c + ext).to_array());
        if let Some(r) = self.clip_xz {
            lo[0] = lo[0].max(r[0]);
            lo[2] = lo[2].max(r[1]);
            hi[0] = hi[0].min(r[2]).max(lo[0]);
            hi[2] = hi[2].min(r[3]).max(lo[2]);
        }
        (lo, hi)
    }

    fn hash_into(&self, h: &mut blake3::Hasher) {
        h.update(b"shape");
        let mut f = |v: f32| {
            h.update(&v.to_bits().to_le_bytes());
        };
        match self.form {
            ShapeForm::Box { half_extents } => {
                f(0.0);
                half_extents.into_iter().for_each(&mut f);
            }
            ShapeForm::Sphere { radius } => {
                f(1.0);
                f(radius);
            }
        }
        self.position.into_iter().for_each(&mut f);
        self.rotation.into_iter().for_each(&mut f);
        f(self.edge_m);
        if let Some(r) = self.clip_xz {
            h.update(b"clip");
            for v in r {
                h.update(&v.to_bits().to_le_bytes());
            }
        }
    }
}

/// A 2D scalar image over an XZ rectangle (spec 62 § 3.3 `Mask`): one channel, row-major, row 0 at
/// `min_z`. The region value is the sample at `p.xz`, 0 outside the rectangle.
#[derive(Clone, Debug, PartialEq)]
pub struct MaskData {
    /// `[min_x, min_z, max_x, max_z]`.
    pub rect: [f32; 4],
    /// Columns.
    pub width: u32,
    /// Rows.
    pub height: u32,
    /// `width * height` values, clamped to [0, 1] when made with [`MaskData::new`].
    pub values: Vec<f32>,
    /// Bilinear (true) or nearest sampling.
    pub bilinear: bool,
    /// One byte per texel, 1 when this mask's biome is the second claim there.
    ///
    /// Empty means the mask has no second-claim channel: [`MaskData::claim_second_at`] is false,
    /// and [`MaskData`] hashes as it did before the channel existed. `biome.areas` fills it.
    /// A scatter point reads the weight with [`MaskData::sample`] (bilinear) and this bit from
    /// the nearest texel.
    pub second: Vec<u8>,
}

impl MaskData {
    /// A mask from values (clamped to [0, 1]; non-finite values become 0).
    ///
    /// # Errors
    ///
    /// A message when the size is zero, the value count does not match, or the rectangle is not
    /// finite with `min <= max` on both axes (audit L11).
    pub fn new(
        rect: [f32; 4],
        width: u32,
        height: u32,
        values: Vec<f32>,
        bilinear: bool,
    ) -> Result<Self, String> {
        let needed = u64::from(width) * u64::from(height);
        if width == 0 || height == 0 || u64::try_from(values.len()).ok() != Some(needed) {
            return Err(format!(
                "mask of {width} x {height} needs {needed} values, has {}",
                values.len()
            ));
        }
        if rect.iter().any(|v| !v.is_finite()) || rect[0] > rect[2] || rect[1] > rect[3] {
            return Err(format!(
                "mask rectangle {rect:?} must be finite [min_x, min_z, max_x, max_z]"
            ));
        }
        let values = values
            .into_iter()
            .map(|v| if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 })
            .collect();
        Ok(Self {
            rect,
            width,
            height,
            values,
            bilinear,
            second: Vec::new(),
        })
    }

    /// Whether the nearest texel records this mask's biome as the second claim.
    ///
    /// False outside the rectangle, and false when [`MaskData::second`] is empty or a different
    /// length than [`MaskData::values`].
    pub fn claim_second_at(&self, x: f32, z: f32) -> bool {
        if self.second.len() != self.values.len() {
            return false;
        }
        let r = self.rect;
        if !(x >= r[0] && x <= r[2] && z >= r[1] && z <= r[3]) {
            return false;
        }
        let (w, h) = (self.width as usize, self.height as usize);
        if w == 0 || h == 0 {
            return false;
        }
        let u = ((x - r[0]) / (r[2] - r[0]).max(1e-9)) * w as f32 - 0.5;
        let v = ((z - r[1]) / (r[3] - r[1]).max(1e-9)) * h as f32 - 0.5;
        let c = (u + 0.5).floor() as i64;
        let row = (v + 0.5).floor() as i64;
        let c = c.clamp(0, w as i64 - 1) as usize;
        let row = row.clamp(0, h as i64 - 1) as usize;
        self.second[row * w + c] != 0
    }

    /// The value at world XZ (0 outside the rectangle).
    pub fn sample(&self, x: f32, z: f32) -> f32 {
        let r = self.rect;
        if !(x >= r[0] && x <= r[2] && z >= r[1] && z <= r[3]) {
            return 0.0;
        }
        let (w, h) = (self.width as usize, self.height as usize);
        let u = ((x - r[0]) / (r[2] - r[0]).max(1e-9)) * w as f32 - 0.5;
        let v = ((z - r[1]) / (r[3] - r[1]).max(1e-9)) * h as f32 - 0.5;
        let at = |c: i64, rr: i64| {
            let c = c.clamp(0, w as i64 - 1) as usize;
            let rr = rr.clamp(0, h as i64 - 1) as usize;
            self.values[rr * w + c]
        };
        if !self.bilinear {
            return at((u + 0.5).floor() as i64, (v + 0.5).floor() as i64);
        }
        let (u0, v0) = (u.floor(), v.floor());
        let (fu, fv) = (u - u0, v - v0);
        let (c0, r0) = (u0 as i64, v0 as i64);
        let a = at(c0, r0) + (at(c0 + 1, r0) - at(c0, r0)) * fu;
        let b = at(c0, r0 + 1) + (at(c0 + 1, r0 + 1) - at(c0, r0 + 1)) * fu;
        a + (b - a) * fv
    }

    fn hash_into(&self, h: &mut blake3::Hasher) {
        h.update(b"mask");
        for v in self.rect {
            h.update(&v.to_bits().to_le_bytes());
        }
        h.update(&self.width.to_le_bytes());
        h.update(&self.height.to_le_bytes());
        h.update(&[u8::from(self.bilinear)]);
        for v in &self.values {
            h.update(&v.to_bits().to_le_bytes());
        }
        // An all-zero or empty channel hashes like a mask from before the channel existed.
        if self.second.iter().any(|b| *b != 0) {
            h.update(b"second");
            h.update(&self.second);
        }
    }
}

/// CPU geometry of a mesh asset (`Mesh` items, spec 63 §§ 3-4): object-space triangles.
#[derive(Clone, Debug, PartialEq)]
pub struct MeshData {
    /// The asset path.
    pub asset: String,
    /// Vertex positions.
    pub positions: Vec<[f32; 3]>,
    /// Vertex normals (same length as `positions`, or empty).
    pub normals: Vec<[f32; 3]>,
    /// Triangle-list indices.
    pub indices: Vec<u32>,
}

impl MeshData {
    /// Local bounds `(min, max)` (zero for an empty mesh).
    pub fn bounds(&self) -> ([f32; 3], [f32; 3]) {
        if self.positions.is_empty() {
            return ([0.0; 3], [0.0; 3]);
        }
        let mut lo = [f32::INFINITY; 3];
        let mut hi = [f32::NEG_INFINITY; 3];
        for p in &self.positions {
            lo = std::array::from_fn(|k| lo[k].min(p[k]));
            hi = std::array::from_fn(|k| hi[k].max(p[k]));
        }
        (lo, hi)
    }

    fn hash_into(&self, h: &mut blake3::Hasher) {
        h.update(b"mesh");
        h.update(self.asset.as_bytes());
        for v in self.positions.iter().chain(&self.normals).flatten() {
            h.update(&v.to_bits().to_le_bytes());
        }
        for i in &self.indices {
            h.update(&i.to_le_bytes());
        }
    }
}

/// A spline item (spec 62 § 3.3 `Spline`, spec 64 § 2): a world-space spline.
#[derive(Clone, Debug, PartialEq)]
pub struct SplineData {
    /// The spline, control points in world space.
    pub spline: regia_scene::spline::Spline,
}

impl SplineData {
    /// A spline item.
    pub fn new(spline: regia_scene::spline::Spline) -> Self {
        Self { spline }
    }

    /// The evaluator.
    pub fn curve(&self) -> regia_scene::spline::SplineCurve {
        self.spline.curve()
    }

    fn hash_into(&self, h: &mut blake3::Hasher) {
        use regia_scene::spline::SplineValue;
        h.update(b"spline");
        let s = &self.spline;
        h.update(&[u8::from(s.closed), s.up as u8]);
        for a in &s.attrs {
            h.update(a.name.as_bytes());
            h.update(&[a.ty as u8]);
        }
        let mut f = |v: f32| {
            h.update(&v.to_bits().to_le_bytes());
        };
        for p in &s.points {
            p.position.to_array().into_iter().for_each(&mut f);
            p.tangent_in.to_array().into_iter().for_each(&mut f);
            p.tangent_out.to_array().into_iter().for_each(&mut f);
            f(p.mode as u8 as f32);
            f(p.roll_deg);
            p.scale.to_array().into_iter().for_each(&mut f);
            for v in &p.values {
                match v {
                    SplineValue::F32(x) => f(*x),
                    SplineValue::Vec3(x) => x.to_array().into_iter().for_each(&mut f),
                    SplineValue::Name(n) => n.bytes().for_each(|b| f(f32::from(b))),
                }
            }
        }
    }
}

/// The payload of one item.
#[derive(Clone, Debug, PartialEq)]
pub enum Payload {
    /// A point set.
    Points(PointSet),
    /// A table.
    Table(Table),
    /// A shape region.
    Shape(ShapeData),
    /// A terrain capture (shared: captures can be large).
    Ground(Arc<GroundData>),
    /// A 2D mask over an XZ rectangle.
    Mask(Arc<MaskData>),
    /// A mesh's CPU geometry.
    Mesh(Arc<MeshData>),
    /// A world-space spline (spec 64 § 2).
    Spline(Arc<SplineData>),
    /// Planar polygon rings (spec 64 § 3).
    Polygon(Arc<crate::polygon::PolygonData>),
    /// Plant skeletons (plant domain, spec 68 § 2).
    PlantSkeleton(Arc<crate::plant::PlantSkeleton>),
    /// A meshed plant (plant domain, spec 68 § 2).
    PlantMesh(Arc<crate::plant::PlantMesh>),
}

impl Payload {
    /// The payload's kind.
    pub fn kind(&self) -> DataKind {
        match self {
            Self::Points(_) => DataKind::Points,
            Self::Table(_) => DataKind::Table,
            Self::Shape(_) => DataKind::Shape,
            Self::Ground(_) => DataKind::Ground,
            Self::Mask(_) => DataKind::Mask,
            Self::Mesh(_) => DataKind::Mesh,
            Self::Spline(_) => DataKind::Spline,
            Self::Polygon(_) => DataKind::Polygon,
            Self::PlantSkeleton(_) => DataKind::PlantSkeleton,
            Self::PlantMesh(_) => DataKind::PlantMesh,
        }
    }

    /// Number of elements (points, rows or plants; 0 for regions).
    pub fn len(&self) -> usize {
        match self {
            Self::Points(p) => p.len(),
            Self::Table(t) => t.rows,
            Self::PlantSkeleton(s) => s.plants.len(),
            Self::PlantMesh(m) => m.skeleton.plants.len(),
            Self::Shape(_)
            | Self::Ground(_)
            | Self::Mask(_)
            | Self::Mesh(_)
            | Self::Spline(_)
            | Self::Polygon(_) => 0,
        }
    }

    /// Whether the payload has no elements.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One unit of data on a wire: a payload, tags and item-scope values (spec 62 § 3.4).
#[derive(Clone, Debug, PartialEq)]
pub struct DataItem {
    /// The payload.
    pub payload: Payload,
    /// Routing tags.
    pub tags: BTreeSet<String>,
    /// Item-scope values (one row).
    pub scope: BTreeMap<String, AttrValue>,
    /// The last attribute a node wrote on this item (`$prev`, spec 62 § 3.5).
    pub last_written: Option<String>,
}

impl DataItem {
    /// An item with no tags or scope.
    pub fn new(payload: Payload) -> Self {
        Self {
            payload,
            tags: BTreeSet::new(),
            scope: BTreeMap::new(),
            last_written: None,
        }
    }

    /// A points item.
    pub fn points(points: PointSet) -> Self {
        Self::new(Payload::Points(points))
    }

    /// The same tags, scope and `$prev` with a new payload.
    pub fn with_payload(&self, payload: Payload) -> Self {
        Self {
            payload,
            tags: self.tags.clone(),
            scope: self.scope.clone(),
            last_written: self.last_written.clone(),
        }
    }

    /// The point set, when the payload is points.
    pub fn as_points(&self) -> Option<&PointSet> {
        match &self.payload {
            Payload::Points(p) => Some(p),
            _ => None,
        }
    }

    /// Feeds the canonical encoding into a hasher.
    pub fn hash_into(&self, h: &mut blake3::Hasher) {
        match &self.payload {
            Payload::Points(p) => p.hash_into(h),
            Payload::Table(t) => t.hash_into(h),
            Payload::Shape(s) => s.hash_into(h),
            Payload::Ground(g) => g.hash_into(h),
            Payload::Mask(m) => m.hash_into(h),
            Payload::Mesh(m) => m.hash_into(h),
            Payload::Spline(sp) => sp.hash_into(h),
            Payload::Polygon(p) => p.hash_into(h),
            Payload::PlantSkeleton(s) => s.hash_into(h),
            Payload::PlantMesh(m) => m.hash_into(h),
        }
        for t in &self.tags {
            h.update(b"tag");
            h.update(t.as_bytes());
        }
        for (k, v) in &self.scope {
            h.update(b"scope");
            h.update(k.as_bytes());
            let mut col = AttrColumn::new(v.ty());
            col.push(v.clone());
            col.hash_into(h);
        }
    }
}

/// The ordered items on one pin in one evaluation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Bundle(pub Vec<DataItem>);

impl Bundle {
    /// An empty bundle.
    pub fn empty() -> Self {
        Self(Vec::new())
    }

    /// A bundle of one item.
    pub fn one(item: DataItem) -> Self {
        Self(vec![item])
    }

    /// Total elements over all items.
    pub fn elements(&self) -> usize {
        self.0.iter().map(|i| i.payload.len()).sum()
    }

    /// The canonical content hash.
    pub fn hash(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(&(self.0.len() as u64).to_le_bytes());
        for item in &self.0 {
            item.hash_into(&mut h);
        }
        *h.finalize().as_bytes()
    }
}

/// A parsed selector (spec 62 § 3.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selector {
    /// `item.` prefix: read the item-scope value.
    pub item_scope: bool,
    /// The attribute or property name (`$prev` already resolved by [`Selector::resolve`]).
    pub name: String,
    /// Component indices picked by a `:xyz` suffix (0..4), empty for the whole value.
    pub parts: Vec<usize>,
}

impl Selector {
    /// Parses `name`, `item.name`, `name:xz`, or `$prev` (also the meaning of an empty string).
    ///
    /// # Errors
    ///
    /// A message for a malformed suffix.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let text = if text.is_empty() { "$prev" } else { text };
        let (base, suffix) = match text.split_once(':') {
            Some((b, s)) => (b, Some(s)),
            None => (text, None),
        };
        let (item_scope, name) = match base.strip_prefix("item.") {
            Some(n) => (true, n),
            None => (false, base),
        };
        let mut parts = Vec::new();
        if let Some(s) = suffix {
            if s.is_empty() || s.len() > 4 {
                return Err(format!("selector `{text}`: 1-4 component letters after `:`"));
            }
            for c in s.chars() {
                parts.push(match c {
                    'x' | 'r' => 0,
                    'y' | 'g' => 1,
                    'z' | 'b' => 2,
                    'w' | 'a' => 3,
                    _ => return Err(format!("selector `{text}`: unknown component `{c}`")),
                });
            }
        }
        if name.is_empty() {
            return Err(format!("selector `{text}` has no name"));
        }
        Ok(Self {
            item_scope,
            name: name.to_string(),
            parts,
        })
    }

    /// Resolves `$prev` against an item.
    pub fn resolve(&self, item: &DataItem) -> Result<Self, String> {
        if self.name != "$prev" {
            return Ok(self.clone());
        }
        match &item.last_written {
            Some(n) => Ok(Self {
                name: n.clone(),
                ..self.clone()
            }),
            None => Err("`$prev`: no attribute was written upstream".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_tables_read_untyped_with_inferred_column_types() {
        let t = table_from_ron(
            r#"(format_version: 1, row_type: "game::WeaponRow", rows: {
                "rifle": (damage: 30, spread: 0.5, kind: Rifle, icon: "ui/rifle.png", tint: (1.0, 0.5, 0.0)),
                "pistol": (damage: 12, spread: 1, kind: Pistol, icon: "ui/pistol.png", tint: (0.0, 0.0, 1.0)),
            })"#,
        )
        .expect("table");
        assert_eq!(t.rows, 2);
        assert_eq!(
            t.column("key"),
            Some(&AttrColumn::Name(vec!["rifle".into(), "pistol".into()].into()))
        );
        assert_eq!(t.column("damage"), Some(&AttrColumn::Int(vec![30, 12].into())));
        assert_eq!(
            t.column("spread"),
            Some(&AttrColumn::Float(vec![0.5, 1.0].into()))
        );
        assert_eq!(
            t.column("kind"),
            Some(&AttrColumn::Name(vec!["Rifle".into(), "Pistol".into()].into()))
        );
        assert!(matches!(t.column("tint"), Some(AttrColumn::Vec3(_))));
        let mixed = table_from_ron(r#"(format_version: 1, rows: { "a": (x: 1), "b": (x: "s") })"#);
        assert!(mixed.is_err_and(|e| e.starts_with("table_column_types")));
        let v0 = table_from_ron(r#"[("a", (n: 1)), ("b", (n: 2))]"#).expect("v0");
        assert_eq!(v0.column("n"), Some(&AttrColumn::Int(vec![1, 2].into())));
    }

    #[test]
    fn selectors_parse_scope_suffix_and_prev() {
        let s = Selector::parse("item.count").expect("parse");
        assert!(s.item_scope);
        assert_eq!(s.name, "count");
        let s = Selector::parse("position:y").expect("parse");
        assert_eq!(s.parts, vec![1]);
        let s = Selector::parse("color:rgb").expect("parse");
        assert_eq!(s.parts, vec![0, 1, 2]);
        assert_eq!(Selector::parse("").expect("parse").name, "$prev");
        assert!(Selector::parse("a:q").is_err());
        assert!(Selector::parse("a:xyzwx").is_err());
    }

    #[test]
    fn append_fills_missing_attributes_and_refuses_conflicts() {
        let mut a = PointSet::new();
        a.push(Point::default());
        a.attrs.insert("w".into(), AttrColumn::Float(vec![2.0].into()));
        let mut b = PointSet::new();
        b.push(Point::default());
        b.attrs.insert("k".into(), AttrColumn::Int(vec![7].into()));
        a.append(&b).expect("append");
        assert_eq!(a.len(), 2);
        assert_eq!(a.attrs["w"], AttrColumn::Float(vec![2.0, 0.0].into()));
        assert_eq!(a.attrs["k"], AttrColumn::Int(vec![0, 7].into()));
        let mut c = PointSet::new();
        c.push(Point::default());
        c.attrs.insert("w".into(), AttrColumn::Int(vec![1].into()));
        assert_eq!(a.append(&c), Err("w".to_string()));
    }

    #[test]
    fn point_region_value_follows_hardness() {
        let mut s = PointSet::new();
        s.push(Point {
            bounds_min: [-1.0; 3],
            bounds_max: [1.0; 3],
            ..Point::default()
        });
        assert_eq!(s.contains(0, [0.9, 0.0, 0.0]), 1.0);
        assert_eq!(s.contains(0, [1.1, 0.0, 0.0]), 0.0);
        s.hardness[0] = 0.0;
        assert!((s.contains(0, [0.5, 0.0, 0.0]) - 0.5).abs() < 1e-6);
        let (lo, hi) = s.world_aabb(0);
        assert_eq!((lo, hi), ([-1.0; 3], [1.0; 3]));
    }

    #[test]
    fn shape_edge_band_falls_off_linearly() {
        let s = ShapeData {
            form: ShapeForm::Box {
                half_extents: [1.0; 3],
            },
            position: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            edge_m: 2.0,
            clip_xz: None,
        };
        assert_eq!(s.value([0.5, 0.0, 0.0]), 1.0);
        assert!((s.value([2.0, 0.0, 0.0]) - 0.5).abs() < 1e-6);
        assert_eq!(s.value([3.5, 0.0, 0.0]), 0.0);
    }

    #[test]
    fn ground_capture_matches_the_terrain_sampler() {
        let n = 33u32;
        let heights: Vec<f32> = (0..n * n).map(|i| ((i * 37) % 101) as f32 * 0.1).collect();
        let map = regia_terrain::TerrainMap::from_heights(n, n, heights, 64.0, 1.0, 0.0).expect("map");
        let g = GroundData::capture(&map, [-10.0, -10.0, 10.0, 10.0], 2);
        for k in 0..200 {
            let x = -10.0 + (k as f32 * 0.37) % 20.0;
            let z = -10.0 + (k as f32 * 0.73) % 20.0;
            assert_eq!(g.height(x, z), map.sample_bilinear(x, z), "height at {x},{z}");
            assert_eq!(g.normal(x, z), map.normal_at(x, z), "normal at {x},{z}");
        }
    }

    /// Audit procgen1-correctness (high): a negative box half extent inverts the capture
    /// rectangle. The unfixed capture then wraps the column count (`u32`) and panics in
    /// `sample`, either on `clamp` with a reversed range or after reserving a multi-gigabyte
    /// empty buffer. The first rectangle is the wrap-to-zero case (no huge reserve). The
    /// shape is the audit's half extents `(-8, 10, 8)`. The last rectangle is the one that
    /// `world_aabb` used to hand `capture` for those extents (`[8, -8, -8, 8]`).
    #[test]
    fn inverted_bounds_do_not_panic_ground_sample() {
        let n = 33u32;
        let heights: Vec<f32> = (0..n * n).map(|i| ((i * 37) % 101) as f32 * 0.1).collect();
        let map = regia_terrain::TerrainMap::from_heights(n, n, heights, 64.0, 1.0, 0.0).expect("map");
        let g = GroundData::capture(&map, [-2.0, 0.0, -12.0, 0.0], 2);
        assert_eq!(
            g.heights.len(),
            g.size[0] as usize * g.size[1] as usize,
            "size {:?} origin {:?} heights {}",
            g.size,
            g.origin,
            g.heights.len()
        );
        let h = g.height(-7.0, 0.0);
        assert!(h.is_finite(), "height {h}");

        let shape = ShapeData {
            form: ShapeForm::Box {
                half_extents: [-8.0, 10.0, 8.0],
            },
            position: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            edge_m: 0.0,
            clip_xz: None,
        };
        let (lo, hi) = shape.world_aabb();
        assert!(
            lo[0] <= hi[0] && lo[1] <= hi[1] && lo[2] <= hi[2],
            "aabb lo {lo:?} hi {hi:?}"
        );
        // Negative half extents clamp to 0, the same rule as `PreparedShape`.
        assert_eq!((lo[0], hi[0]), (0.0, 0.0));
        assert_eq!((lo[2], hi[2]), (-8.0, 8.0));
        assert_eq!(shape.value([0.0, 0.0, 0.0]), 1.0);
        assert_eq!(shape.value([1.0, 0.0, 0.0]), 0.0);
        let rect = [lo[0], lo[2], hi[0], hi[2]];
        let g = GroundData::capture(&map, rect, 2);
        assert_eq!(g.heights.len(), g.size[0] as usize * g.size[1] as usize);
        assert!(g.height(0.0, 0.0).is_finite());

        let g = GroundData::capture(&map, [8.0, -8.0, -8.0, 8.0], 2);
        assert_eq!(g.heights.len(), g.size[0] as usize * g.size[1] as usize);
        let h = g.height(0.0, 0.0);
        assert!(h.is_finite(), "height {h}");

        let empty = GroundData {
            rect: [0.0; 4],
            map_size: [n, n],
            world_width: 64.0,
            origin: [0, 0],
            size: [0, 0],
            heights: Vec::new(),
            layers: Vec::new(),
        };
        assert_eq!(empty.height(0.0, 0.0), 0.0);
        let empty_map = GroundData {
            map_size: [0, 0],
            size: [0, 0],
            ..empty
        };
        assert_eq!(empty_map.height(0.0, 0.0), 0.0);
        assert_eq!(empty_map.normal(0.0, 0.0), [0.0, 1.0, 0.0]);
    }

    /// Audit procgen1-correctness (low, same file): a non-finite `bounds_max` is named
    /// `bounds_max`, not `bounds_min`.
    #[test]
    fn non_finite_bounds_max_is_named_bounds_max() {
        let mut s = PointSet::new();
        s.push(Point::default());
        s.bounds_max[0][0] = f32::NAN;
        assert_eq!(s.first_non_finite(), Some(("bounds_max".to_string(), 0)));
    }
}
