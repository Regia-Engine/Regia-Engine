// SPDX-License-Identifier: MPL-2.0
//! A standalone snapshot delta codec: quantised, schema-driven world snapshots sent as deltas
//! against an acknowledged baseline.
//!
//! This layer is deliberately independent of the ECS replication in `crate::replication`. A game
//! (or a tool, a replay system, a state-sync prototype) describes its per-entity state once as a
//! [`SnapshotSchema`], hands whole [`WorldSnapshot`]s to a [`SnapshotEncoder`], and gets back one
//! self-framing packet per call. The peer feeds packets to a [`SnapshotDecoder`] and reads the
//! reconstructed snapshot out of every [`DecodedSnapshot`].
//!
//! The model is the acknowledged-baseline one from Bernier (2001) and Fiedler's "Snapshot
//! Compression" (see `docs/provenance.md`):
//!
//! - Every packet carries a sequence number ([`Seq`]) and names the baseline it was encoded
//!   against: the last sequence the client acknowledged.
//! - A lost packet costs nothing. The client keeps acknowledging the older sequence, so the next
//!   packet is a delta against the state the client still holds.
//! - When the acknowledged baseline has fallen out of the encoder's ring buffer (or there is no
//!   ack yet), the encoder falls back to a full snapshot.
//! - Values are quantised before comparison, so a change smaller than one quantisation step is
//!   not a change at all: the change mask only ever names fields whose wire bits differ.
//! - Encoding honours a byte budget. Records are packed by descending entity priority; what does
//!   not fit is reported in [`EncodeReport::omitted`] and the packet is marked incomplete. Both
//!   sides store every packet as a baseline holding exactly what it carried: the acknowledged
//!   baseline with the included records applied, omitted entities left at their baseline state.
//!   A budget overflow therefore defers an update but never corrupts the reconstructed state, and
//!   each acknowledged packet is progress, so a world larger than one packet still converges.
//!
//! # Wire format
//!
//! ```text
//! packet  := magic u8 | version u8 | total_len u32le | crc32 u32le | body
//! body    := schema_hash u32 | flags u8 | seq varint | tick varint
//!            | baseline varint (0 = full snapshot) | count varint | record*
//! record  := kind u2 | id varint | payload
//! kind    := 0 add | 1 update | 2 remove
//! add     := every field, in schema order
//! update  := change mask (one bit per field) | the changed fields, in schema order
//! remove  := (nothing)
//! flags   := bit 0 full snapshot, bit 1 complete (nothing omitted for budget)
//! ```
//!
//! The body is bit-packed with [`crate::bits::BitWriter`] (least-significant bit first). The
//! outer header is byte-aligned: `total_len` makes every truncation a length mismatch and the
//! IEEE CRC-32 over the body catches corruption, so damaged input is an error, never a panic.
//! Quantised field encodings reuse [`crate::codec::Quantize`] and the smallest-three
//! [`crate::codec::Rotation`], so the bytes agree with the rest of the crate's replication codec
//! and are identical on every platform for the same inputs.
//!
//! # Failure behaviour
//!
//! Decoding untrusted bytes never panics: short, truncated, corrupt, schema-mismatched or
//! baseline-less packets return a [`SnapshotError`] and leave the decoder's state untouched.
//! Encoding validates the snapshot against the schema and reports type mismatches, duplicate
//! entities and out-of-range enum discriminants as errors. Non-finite floats are not errors:
//! a NaN or infinite component quantises to the nearest end of its range, and a zero or
//! non-finite quaternion becomes the identity rotation, matching the crate codec.

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

use glam::{Quat, Vec3};
use thiserror::Error;

use crate::bits::{BitReader, BitWriter, DecodeError, varint_bits};
use crate::codec::{Quantize, Rotation};
use crate::model::{NetId, NetTick};

/// First byte of every snapshot packet.
pub const PACKET_MAGIC: u8 = 0xd1;
/// The only packet version this codec writes and reads.
pub const PACKET_VERSION: u8 = 1;
/// Bytes of the byte-aligned outer header (`magic | version | total_len | crc32`).
pub const OUTER_HEADER_BYTES: usize = 10;
/// Most fields one [`SnapshotSchema`] may declare.
pub const MAX_FIELDS: usize = 128;
/// Most entities one [`WorldSnapshot`] (and one packet) may carry.
pub const MAX_ENTITIES: usize = 65_536;
/// Baselines the encoder and decoder ring buffers hold unless configured otherwise.
pub const DEFAULT_BASELINE_CAPACITY: usize = 64;

const RECORD_ADD: u64 = 0;
const RECORD_UPDATE: u64 = 1;
const RECORD_REMOVE: u64 = 2;

const FLAG_FULL: u64 = 0b01;
const FLAG_COMPLETE: u64 = 0b10;

/// A snapshot packet sequence number. Assigned by [`SnapshotEncoder`], starting at 1 and
/// increasing by one per encoded packet (`Seq(0)` is never assigned; on the wire 0 means
/// "no baseline").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Seq(pub u32);

/// How one field of a snapshot travels on the wire.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FieldKind {
    /// One bit.
    Bool,
    /// Eight bits, exact.
    U8,
    /// Sixteen bits, exact.
    U16,
    /// Thirty-two bits, exact.
    U32,
    /// A zig-zag varint: small magnitudes stay small (8 to 40 bits).
    I32,
    /// An `f32` quantised to `bits` bits over `[min, max]`. Values outside the range (and NaN)
    /// clamp to the nearer end.
    Float {
        /// Lowest representable value.
        min: f32,
        /// Highest representable value.
        max: f32,
        /// Bits on the wire (1..=32).
        bits: u32,
    },
    /// A [`Vec3`] with each axis quantised to `bits` bits over its own `[min, max]` component
    /// range. Out-of-range and NaN components clamp per axis.
    Vec3Quantised {
        /// Lowest representable value per axis.
        min: Vec3,
        /// Highest representable value per axis.
        max: Vec3,
        /// Bits per axis (1..=32).
        bits: u32,
    },
    /// A [`Quat`] as smallest-three: 2 bits for the dropped (largest) component plus `bits` bits
    /// for each kept component. A zero or non-finite quaternion travels as the identity.
    QuatSmallestThree {
        /// Bits per kept component (2..=32, 10 is a good default).
        bits: u32,
    },
    /// An enum discriminant in `0..variants`, in the smallest whole number of bits that holds
    /// every variant.
    Enum {
        /// Number of variants (at least 2).
        variants: u32,
    },
}

impl FieldKind {
    /// Short lowercase name for errors and logs.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::I32 => "i32",
            Self::Float { .. } => "f32",
            Self::Vec3Quantised { .. } => "vec3",
            Self::QuatSmallestThree { .. } => "quat",
            Self::Enum { .. } => "enum",
        }
    }

    /// Bits one value of this kind occupies, or `None` when the width depends on the value
    /// (only [`FieldKind::I32`], a zig-zag varint of 8 to 40 bits).
    pub fn wire_bits(&self) -> Option<u32> {
        match self {
            Self::Bool => Some(1),
            Self::U8 => Some(8),
            Self::U16 => Some(16),
            Self::U32 => Some(32),
            Self::I32 => None,
            Self::Float { bits, .. } => Some((*bits).clamp(1, 32)),
            Self::Vec3Quantised { bits, .. } => Some(3 * (*bits).clamp(1, 32)),
            Self::QuatSmallestThree { bits } => Some(2 + 3 * (*bits).clamp(2, 32)),
            Self::Enum { variants } => Some(enum_bits(*variants)),
        }
    }

    /// The largest absolute error quantisation can introduce for an in-range value, per scalar
    /// component: half a quantisation step plus the `f32` rounding of the quantiser's arithmetic
    /// (`8 * (span + max(|min|, |max|)) * f32::EPSILON`, which dominates above about 22 bits) for
    /// [`FieldKind::Float`] and (per axis, the widest axis) for [`FieldKind::Vec3Quantised`],
    /// zero for the exact kinds, and `None` for [`FieldKind::QuatSmallestThree`], whose error is
    /// angular rather than per-component.
    pub fn max_quantisation_error(&self) -> Option<f32> {
        match self {
            Self::Bool | Self::U8 | Self::U16 | Self::U32 | Self::I32 | Self::Enum { .. } => Some(0.0),
            Self::Float { min, max, bits } => Some(axis_error_bound(*min, *max, *bits)),
            Self::Vec3Quantised { min, max, bits } => {
                let x = axis_error_bound(min.x, max.x, *bits);
                let y = axis_error_bound(min.y, max.y, *bits);
                let z = axis_error_bound(min.z, max.z, *bits);
                Some(x.max(y).max(z))
            }
            Self::QuatSmallestThree { .. } => None,
        }
    }
}

/// The error bound of one quantised axis: half of one quantisation step of a `bits`-bit
/// fixed-point value over `[min, max]`, plus the rounding of the `f32` subtract, scale, round,
/// convert, multiply and add the quantiser and dequantiser perform (each at most half an ulp of
/// a value no larger than the span or the range ends; 8 epsilons covers them with margin).
fn axis_error_bound(min: f32, max: f32, bits: u32) -> f32 {
    let steps = ((1u64 << bits.clamp(1, 32)) - 1) as f32;
    let span = max - min;
    let half_step = if steps <= 0.0 { 0.0 } else { span / (2.0 * steps) };
    let rounding = 8.0 * (span + min.abs().max(max.abs())) * f32::EPSILON;
    half_step + rounding
}

/// Bits an enum discriminant of `variants` variants occupies (at least 1).
fn enum_bits(variants: u32) -> u32 {
    match variants {
        0 | 1 => 1,
        v => 32 - (v - 1).leading_zeros(),
    }
}

/// One named field of a [`SnapshotSchema`].
#[derive(Debug, Clone, PartialEq)]
pub struct FieldDef {
    /// The field's name. Names are part of the schema hash, so renaming a field is a wire break.
    pub name: String,
    /// How the field travels.
    pub kind: FieldKind,
}

impl FieldDef {
    /// A field definition from a name and a kind.
    pub fn new(name: impl Into<String>, kind: FieldKind) -> Self {
        Self {
            name: name.into(),
            kind,
        }
    }
}

/// One field's value in an [`EntitySnapshot`]. The variant must match the [`FieldKind`] the
/// schema declares at the same position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FieldValue {
    /// A [`FieldKind::Bool`] value.
    Bool(bool),
    /// A [`FieldKind::U8`] value.
    U8(u8),
    /// A [`FieldKind::U16`] value.
    U16(u16),
    /// A [`FieldKind::U32`] value.
    U32(u32),
    /// A [`FieldKind::I32`] value.
    I32(i32),
    /// A [`FieldKind::Float`] value (quantised on the wire).
    F32(f32),
    /// A [`FieldKind::Vec3Quantised`] value.
    Vec3(Vec3),
    /// A [`FieldKind::QuatSmallestThree`] value.
    Quat(Quat),
    /// A [`FieldKind::Enum`] discriminant.
    Enum(u32),
}

impl FieldValue {
    /// Short lowercase name of the value's type, for errors and logs.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Bool(_) => "bool",
            Self::U8(_) => "u8",
            Self::U16(_) => "u16",
            Self::U32(_) => "u32",
            Self::I32(_) => "i32",
            Self::F32(_) => "f32",
            Self::Vec3(_) => "vec3",
            Self::Quat(_) => "quat",
            Self::Enum(_) => "enum",
        }
    }
}

/// Why a schema, a snapshot or a packet could not be encoded or decoded.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum SnapshotError {
    /// A schema with no fields.
    #[error("snapshot schema declares no fields")]
    EmptySchema,
    /// A schema with more than [`MAX_FIELDS`] fields.
    #[error("snapshot schema declares {count} fields, maximum is {max}")]
    TooManyFields {
        /// Fields declared.
        count: usize,
        /// Allowed maximum.
        max: usize,
    },
    /// Two fields share a name.
    #[error("duplicate field name in snapshot schema: {0}")]
    DuplicateField(String),
    /// A field definition is not usable (bad range, bad bit width, empty name, too few variants).
    #[error("invalid field {field:?}: {reason}")]
    InvalidField {
        /// The field's name.
        field: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// A field index outside the schema.
    #[error("field index {index} outside a schema of {count} fields")]
    InvalidFieldIndex {
        /// The index asked for.
        index: usize,
        /// Fields in the schema.
        count: usize,
    },
    /// A snapshot with more than [`MAX_ENTITIES`] entities.
    #[error("world snapshot holds {count} entities, maximum is {max}")]
    TooManyEntities {
        /// Entities present.
        count: usize,
        /// Allowed maximum.
        max: usize,
    },
    /// The same entity id twice in one snapshot.
    #[error("duplicate entity {0:?} in world snapshot")]
    DuplicateEntity(NetId),
    /// An entity whose field count differs from the schema.
    #[error("entity {entity:?} has {got} fields, the schema declares {expected}")]
    FieldCountMismatch {
        /// The offending entity.
        entity: NetId,
        /// Fields the schema declares.
        expected: usize,
        /// Fields the entity carried.
        got: usize,
    },
    /// A value whose type does not match the field's declared kind.
    #[error("field {field} expects {expected}, value is {got}")]
    FieldTypeMismatch {
        /// Index of the field in the schema.
        field: usize,
        /// The declared kind's name.
        expected: &'static str,
        /// The value's type name.
        got: &'static str,
    },
    /// An enum discriminant at or past the variant count.
    #[error("field {field} enum discriminant {value} outside 0..{variants}")]
    EnumOutOfRange {
        /// Index of the field in the schema.
        field: usize,
        /// The discriminant supplied.
        value: u32,
        /// The declared variant count.
        variants: u32,
    },
    /// The byte budget cannot even hold a packet header.
    #[error("budget of {budget} bytes is below the {minimum}-byte minimum packet")]
    BudgetTooSmall {
        /// Budget supplied.
        budget: usize,
        /// Smallest packet this call could have produced.
        minimum: usize,
    },
    /// The assembled packet would exceed the `u32` length prefix.
    #[error("snapshot packet of {len} bytes exceeds the length prefix")]
    PacketTooLarge {
        /// Packet length in bytes.
        len: usize,
    },
    /// Fewer bytes than the outer header.
    #[error("snapshot packet of {len} bytes is shorter than the {minimum}-byte header")]
    TooShort {
        /// Bytes supplied.
        len: usize,
        /// Header size.
        minimum: usize,
    },
    /// The first byte is not [`PACKET_MAGIC`].
    #[error("bad snapshot packet magic {got:#04x}")]
    BadMagic {
        /// The byte found.
        got: u8,
    },
    /// A packet version this codec does not read.
    #[error("unsupported snapshot packet version {got}")]
    UnsupportedVersion {
        /// The version byte found.
        got: u8,
    },
    /// The length prefix disagrees with the slice length (truncated or extended packet).
    #[error("snapshot packet declares {declared} bytes, slice holds {actual}")]
    LengthMismatch {
        /// The `total_len` prefix.
        declared: usize,
        /// The slice length.
        actual: usize,
    },
    /// The body's CRC-32 does not match the header's.
    #[error("snapshot packet CRC mismatch: header {expected:#010x}, computed {computed:#010x}")]
    CrcMismatch {
        /// The CRC stored in the header.
        expected: u32,
        /// The CRC of the body as received.
        computed: u32,
    },
    /// The packet was encoded against a different schema.
    #[error("snapshot schema mismatch: this decoder is {expected:#010x}, packet is {got:#010x}")]
    SchemaMismatch {
        /// This decoder's schema hash.
        expected: u32,
        /// The packet's schema hash.
        got: u32,
    },
    /// Header bits that no encoder writes (reserved flags, a zero sequence, a full snapshot
    /// naming a baseline, non-zero padding, an out-of-range integer).
    #[error("invalid snapshot packet header: {0}")]
    InvalidHeader(&'static str),
    /// A record no encoder writes (reserved kind, an update or remove for an entity the baseline
    /// does not hold, a duplicate id, an out-of-range decoded value).
    #[error("invalid snapshot record: {0}")]
    InvalidRecord(&'static str),
    /// Whole unread bytes after the last record.
    #[error("snapshot packet has {bits} trailing bits after the last record")]
    TrailingBits {
        /// Unread bits.
        bits: usize,
    },
    /// A delta whose baseline this decoder no longer holds. The decoder's state is unchanged;
    /// the caller should keep acknowledging its newest held sequence so the server re-bases or
    /// sends a full snapshot.
    #[error("snapshot baseline {seq:?} is not held by this decoder")]
    BaselineMissing {
        /// The baseline the packet named.
        seq: Seq,
    },
    /// The bit stream ended early or declared an impossible length.
    #[error("snapshot bit stream: {0}")]
    Bits(#[from] DecodeError),
}

/// IEEE CRC-32 (polynomial `0xEDB8_8320`, reflected, init and final XOR `0xFFFF_FFFF`) of
/// `bytes`, the same checksum as Ethernet and `zlib.crc32`. Guards every packet body.
pub fn crc32_ieee(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xffff_ffff;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

/// The field layout every snapshot in one codec session follows: an ordered list of named
/// fields. Both peers must build the identical schema; the [`SnapshotSchema::layout_hash`]
/// travels in every packet so a mismatch is an error, not silent corruption.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotSchema {
    fields: Vec<FieldDef>,
    hash: u32,
}

impl SnapshotSchema {
    /// A schema from its fields, in wire order. Validates names, ranges and bit widths.
    pub fn new(fields: Vec<FieldDef>) -> Result<Self, SnapshotError> {
        if fields.is_empty() {
            return Err(SnapshotError::EmptySchema);
        }
        if fields.len() > MAX_FIELDS {
            return Err(SnapshotError::TooManyFields {
                count: fields.len(),
                max: MAX_FIELDS,
            });
        }
        let mut names: BTreeSet<&str> = BTreeSet::new();
        for field in &fields {
            if field.name.is_empty() {
                return Err(SnapshotError::InvalidField {
                    field: field.name.clone(),
                    reason: "field name is empty",
                });
            }
            if !names.insert(field.name.as_str()) {
                return Err(SnapshotError::DuplicateField(field.name.clone()));
            }
            validate_kind(field)?;
        }
        let hash = compute_layout_hash(&fields);
        Ok(Self { fields, hash })
    }

    /// The fields, in wire order.
    pub fn fields(&self) -> &[FieldDef] {
        &self.fields
    }

    /// How many fields the schema declares.
    pub fn field_count(&self) -> usize {
        self.fields.len()
    }

    /// The field at `index`, if any.
    pub fn field(&self, index: usize) -> Option<&FieldDef> {
        self.fields.get(index)
    }

    /// A 32-bit fingerprint of the whole layout (names, kinds, ranges, widths, variant counts),
    /// carried in every packet. It is a layout guard, not a security hash.
    pub fn layout_hash(&self) -> u32 {
        self.hash
    }

    /// The index of the field called `name`, if the schema declares one.
    pub fn field_index(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.name == name)
    }

    /// What `value` becomes after a trip through the wire for the field at `field_index`:
    /// quantised (and clamped) exactly as a decoder will reconstruct it. Exact kinds return the
    /// value unchanged.
    pub fn quantize_value(
        &self,
        field_index: usize,
        value: &FieldValue,
    ) -> Result<FieldValue, SnapshotError> {
        let Some(field) = self.fields.get(field_index) else {
            return Err(SnapshotError::InvalidFieldIndex {
                index: field_index,
                count: self.fields.len(),
            });
        };
        let key = key_of(&field.kind, field_index, value)?;
        Ok(value_of(&field.kind, key))
    }

    /// [`SnapshotSchema::quantize_value`] applied to every field of every entity: the snapshot a
    /// decoder reconstructs from a complete packet encoding `world`. Validates the snapshot the
    /// same way encoding does.
    pub fn quantize_snapshot(&self, world: &WorldSnapshot) -> Result<WorldSnapshot, SnapshotError> {
        if world.entities.len() > MAX_ENTITIES {
            return Err(SnapshotError::TooManyEntities {
                count: world.entities.len(),
                max: MAX_ENTITIES,
            });
        }
        let mut seen: BTreeSet<NetId> = BTreeSet::new();
        let mut entities = Vec::with_capacity(world.entities.len());
        for entity in &world.entities {
            if !seen.insert(entity.id) {
                return Err(SnapshotError::DuplicateEntity(entity.id));
            }
            let keys = self.entity_keys(entity)?;
            let values = self
                .fields
                .iter()
                .zip(keys)
                .map(|(def, key)| value_of(&def.kind, key))
                .collect();
            entities.push(EntitySnapshot {
                id: entity.id,
                priority: entity.priority,
                fields: values,
            });
        }
        Ok(WorldSnapshot { entities })
    }

    /// The wire keys of one entity, after shape validation. The encoder needs only these; the
    /// dequantised values are built only where a caller asks for them.
    fn entity_keys(&self, entity: &EntitySnapshot) -> Result<Vec<WireKey>, SnapshotError> {
        let mut keys = Vec::with_capacity(entity.fields.len());
        self.entity_keys_into(entity, &mut keys)?;
        Ok(keys)
    }

    /// [`SnapshotSchema::entity_keys`] appended to `out`, so the encoder reuses one buffer for the
    /// whole world instead of allocating per entity (spec 120 § 8.1, R1). On error `out` may hold
    /// a partial entity; callers clear it.
    fn entity_keys_into(&self, entity: &EntitySnapshot, out: &mut Vec<WireKey>) -> Result<(), SnapshotError> {
        if entity.fields.len() != self.fields.len() {
            return Err(SnapshotError::FieldCountMismatch {
                entity: entity.id,
                expected: self.fields.len(),
                got: entity.fields.len(),
            });
        }
        for (index, (def, value)) in self.fields.iter().zip(entity.fields.iter()).enumerate() {
            out.push(key_of(&def.kind, index, value)?);
        }
        Ok(())
    }
}

/// Rejects a field kind no encoder could write.
fn validate_kind(field: &FieldDef) -> Result<(), SnapshotError> {
    let invalid = |reason: &'static str| SnapshotError::InvalidField {
        field: field.name.clone(),
        reason,
    };
    match &field.kind {
        FieldKind::Bool | FieldKind::U8 | FieldKind::U16 | FieldKind::U32 | FieldKind::I32 => Ok(()),
        FieldKind::Float { min, max, bits } => {
            if !min.is_finite() || !max.is_finite() || min >= max {
                return Err(invalid("float range must be finite with min < max"));
            }
            if !(1..=32).contains(bits) {
                return Err(invalid("float bits must be in 1..=32"));
            }
            Ok(())
        }
        FieldKind::Vec3Quantised { min, max, bits } => {
            let mins = [min.x, min.y, min.z];
            let maxs = [max.x, max.y, max.z];
            let finite = mins.iter().chain(maxs.iter()).all(|c| c.is_finite());
            let ordered = mins.iter().zip(maxs.iter()).all(|(lo, hi)| lo < hi);
            if !finite || !ordered {
                return Err(invalid("vec3 range must be finite with min < max on every axis"));
            }
            if !(1..=32).contains(bits) {
                return Err(invalid("vec3 bits must be in 1..=32"));
            }
            Ok(())
        }
        FieldKind::QuatSmallestThree { bits } => {
            if !(2..=32).contains(bits) {
                return Err(invalid("quat bits must be in 2..=32"));
            }
            Ok(())
        }
        FieldKind::Enum { variants } => {
            if *variants < 2 {
                return Err(invalid("enum must declare at least 2 variants"));
            }
            Ok(())
        }
    }
}

/// The canonical byte description of a layout, CRC-32'd into [`SnapshotSchema::layout_hash`].
fn compute_layout_hash(fields: &[FieldDef]) -> u32 {
    let mut bytes: Vec<u8> = Vec::new();
    for field in fields {
        bytes.extend_from_slice(field.name.as_bytes());
        bytes.push(0);
        match &field.kind {
            FieldKind::Bool => bytes.push(1),
            FieldKind::U8 => bytes.push(2),
            FieldKind::U16 => bytes.push(3),
            FieldKind::U32 => bytes.push(4),
            FieldKind::I32 => bytes.push(5),
            FieldKind::Float { min, max, bits } => {
                bytes.push(6);
                bytes.extend_from_slice(&min.to_bits().to_le_bytes());
                bytes.extend_from_slice(&max.to_bits().to_le_bytes());
                bytes.extend_from_slice(&bits.to_le_bytes());
            }
            FieldKind::Vec3Quantised { min, max, bits } => {
                bytes.push(7);
                for component in [min.x, min.y, min.z, max.x, max.y, max.z] {
                    bytes.extend_from_slice(&component.to_bits().to_le_bytes());
                }
                bytes.extend_from_slice(&bits.to_le_bytes());
            }
            FieldKind::QuatSmallestThree { bits } => {
                bytes.push(8);
                bytes.extend_from_slice(&bits.to_le_bytes());
            }
            FieldKind::Enum { variants } => {
                bytes.push(9);
                bytes.extend_from_slice(&variants.to_le_bytes());
            }
        }
    }
    crc32_ieee(&bytes)
}

/// The exact integers one field value travels as. Change detection compares keys, so a change
/// below one quantisation step never reaches the wire, and encoding writes keys back out
/// verbatim, so decoding reproduces `value_of(key)` bit for bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WireKey {
    /// Bool, unsigned ints, enum discriminants and quantised floats.
    U(u64),
    /// Signed ints before zig-zag mapping.
    I(i64),
    /// The three quantised axes of a vec3.
    Triple([u64; 3]),
    /// A smallest-three quaternion: dropped index and the three kept components.
    Quat(u64, [u64; 3]),
}

impl WireKey {
    fn as_u64(self) -> u64 {
        match self {
            Self::U(v) => v,
            _ => 0,
        }
    }

    fn as_i64(self) -> i64 {
        match self {
            Self::I(v) => v,
            _ => 0,
        }
    }

    fn as_triple(self) -> [u64; 3] {
        match self {
            Self::Triple(v) => v,
            _ => [0; 3],
        }
    }

    fn as_quat(self) -> (u64, [u64; 3]) {
        match self {
            Self::Quat(largest, kept) => (largest, kept),
            _ => (0, [0; 3]),
        }
    }
}

/// The crate codec's scalar quantiser for one axis.
fn quantizer(min: f32, max: f32, bits: u32) -> Quantize {
    Quantize {
        min,
        max,
        bits: bits.clamp(1, 32),
    }
}

/// The wire key of a value under a field kind (`field` is only for error reporting).
fn key_of(kind: &FieldKind, field: usize, value: &FieldValue) -> Result<WireKey, SnapshotError> {
    let mismatch = || SnapshotError::FieldTypeMismatch {
        field,
        expected: kind.name(),
        got: value.kind_name(),
    };
    match (kind, value) {
        (FieldKind::Bool, FieldValue::Bool(v)) => Ok(WireKey::U(u64::from(*v))),
        (FieldKind::U8, FieldValue::U8(v)) => Ok(WireKey::U(u64::from(*v))),
        (FieldKind::U16, FieldValue::U16(v)) => Ok(WireKey::U(u64::from(*v))),
        (FieldKind::U32, FieldValue::U32(v)) => Ok(WireKey::U(u64::from(*v))),
        (FieldKind::I32, FieldValue::I32(v)) => Ok(WireKey::I(i64::from(*v))),
        (FieldKind::Float { min, max, bits }, FieldValue::F32(v)) => {
            Ok(WireKey::U(quantizer(*min, *max, *bits).quantize(*v).0))
        }
        (FieldKind::Vec3Quantised { min, max, bits }, FieldValue::Vec3(v)) => Ok(WireKey::Triple([
            quantizer(min.x, max.x, *bits).quantize(v.x).0,
            quantizer(min.y, max.y, *bits).quantize(v.y).0,
            quantizer(min.z, max.z, *bits).quantize(v.z).0,
        ])),
        (FieldKind::QuatSmallestThree { bits }, FieldValue::Quat(v)) => {
            let (largest, kept) = Rotation { bits: *bits }.quantize(*v);
            Ok(WireKey::Quat(largest, kept))
        }
        (FieldKind::Enum { variants }, FieldValue::Enum(v)) => {
            if *v >= *variants {
                return Err(SnapshotError::EnumOutOfRange {
                    field,
                    value: *v,
                    variants: *variants,
                });
            }
            Ok(WireKey::U(u64::from(*v)))
        }
        _ => Err(mismatch()),
    }
}

/// The (quantised) value a wire key stands for.
fn value_of(kind: &FieldKind, key: WireKey) -> FieldValue {
    match kind {
        FieldKind::Bool => FieldValue::Bool(key.as_u64() != 0),
        FieldKind::U8 => FieldValue::U8(key.as_u64() as u8),
        FieldKind::U16 => FieldValue::U16(key.as_u64() as u16),
        FieldKind::U32 => FieldValue::U32(key.as_u64() as u32),
        FieldKind::I32 => FieldValue::I32(key.as_i64() as i32),
        FieldKind::Float { min, max, bits } => {
            FieldValue::F32(quantizer(*min, *max, *bits).dequantize(key.as_u64()))
        }
        FieldKind::Vec3Quantised { min, max, bits } => {
            let q = key.as_triple();
            FieldValue::Vec3(Vec3::new(
                quantizer(min.x, max.x, *bits).dequantize(q[0]),
                quantizer(min.y, max.y, *bits).dequantize(q[1]),
                quantizer(min.z, max.z, *bits).dequantize(q[2]),
            ))
        }
        FieldKind::QuatSmallestThree { bits } => {
            let (largest, kept) = key.as_quat();
            FieldValue::Quat(Rotation { bits: *bits }.dequantize(largest, kept))
        }
        FieldKind::Enum { .. } => FieldValue::Enum(key.as_u64() as u32),
    }
}

/// Appends a wire key under its field kind.
fn write_key(kind: &FieldKind, key: WireKey, w: &mut BitWriter) {
    match kind {
        FieldKind::Bool => w.write_bool(key.as_u64() != 0),
        FieldKind::U8 => w.write_bits(key.as_u64(), 8),
        FieldKind::U16 => w.write_bits(key.as_u64(), 16),
        FieldKind::U32 => w.write_bits(key.as_u64(), 32),
        FieldKind::I32 => w.write_zigzag(key.as_i64()),
        FieldKind::Float { bits, .. } => w.write_bits(key.as_u64(), (*bits).clamp(1, 32)),
        FieldKind::Vec3Quantised { bits, .. } => {
            for axis in key.as_triple() {
                w.write_bits(axis, (*bits).clamp(1, 32));
            }
        }
        FieldKind::QuatSmallestThree { bits } => {
            let (largest, kept) = key.as_quat();
            w.write_bits(largest, 2);
            for component in kept {
                w.write_bits(component, (*bits).clamp(2, 32));
            }
        }
        FieldKind::Enum { variants } => w.write_bits(key.as_u64(), enum_bits(*variants)),
    }
}

/// Reads a wire key under its field kind, rejecting values no encoder writes.
fn read_key(kind: &FieldKind, r: &mut BitReader<'_>) -> Result<WireKey, SnapshotError> {
    match kind {
        FieldKind::Bool => Ok(WireKey::U(r.read_bits(1)?)),
        FieldKind::U8 => Ok(WireKey::U(r.read_bits(8)?)),
        FieldKind::U16 => Ok(WireKey::U(r.read_bits(16)?)),
        FieldKind::U32 => Ok(WireKey::U(r.read_bits(32)?)),
        FieldKind::I32 => {
            let raw = r.read_zigzag()?;
            let value =
                i32::try_from(raw).map_err(|_| SnapshotError::InvalidRecord("i32 field out of range"))?;
            Ok(WireKey::I(i64::from(value)))
        }
        FieldKind::Float { bits, .. } => Ok(WireKey::U(r.read_bits((*bits).clamp(1, 32))?)),
        FieldKind::Vec3Quantised { bits, .. } => {
            let width = (*bits).clamp(1, 32);
            Ok(WireKey::Triple([
                r.read_bits(width)?,
                r.read_bits(width)?,
                r.read_bits(width)?,
            ]))
        }
        FieldKind::QuatSmallestThree { bits } => {
            let width = (*bits).clamp(2, 32);
            let largest = r.read_bits(2)?;
            let kept = [r.read_bits(width)?, r.read_bits(width)?, r.read_bits(width)?];
            Ok(WireKey::Quat(largest, kept))
        }
        FieldKind::Enum { variants } => {
            let raw = r.read_bits(enum_bits(*variants))?;
            if raw >= u64::from(*variants) {
                return Err(SnapshotError::InvalidRecord("enum discriminant out of range"));
            }
            Ok(WireKey::U(raw))
        }
    }
}

/// One entity's state at one tick: its id, its send priority and one value per schema field.
#[derive(Debug, Clone, PartialEq)]
pub struct EntitySnapshot {
    /// The entity's network id.
    pub id: NetId,
    /// Send priority under a byte budget: higher is packed first. Ties break on the lower id,
    /// so packing never depends on input order. Priorities are local to the encoder and do not
    /// travel on the wire; a reconstructed snapshot reports 0.
    pub priority: u32,
    /// One value per field of the schema, in schema order.
    pub fields: Vec<FieldValue>,
}

impl EntitySnapshot {
    /// An entity snapshot with priority 0.
    pub fn new(id: NetId, fields: Vec<FieldValue>) -> Self {
        Self {
            id,
            priority: 0,
            fields,
        }
    }

    /// The same snapshot with a send priority (builder style).
    pub fn with_priority(mut self, priority: u32) -> Self {
        self.priority = priority;
        self
    }
}

/// The whole replicated world at one tick: the unit an encoder consumes and a decoder
/// reconstructs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorldSnapshot {
    /// Every entity's state. Order is not significant: encoding validates into id order
    /// internally, and decoding always yields entities sorted by id.
    pub entities: Vec<EntitySnapshot>,
}

impl WorldSnapshot {
    /// A snapshot from its entities.
    pub fn new(entities: Vec<EntitySnapshot>) -> Self {
        Self { entities }
    }

    /// An empty snapshot (no entities).
    pub fn empty() -> Self {
        Self::default()
    }

    /// How many entities the snapshot holds.
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    /// Whether the snapshot holds no entities.
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// The entity with `id`, if present.
    pub fn get(&self, id: NetId) -> Option<&EntitySnapshot> {
        self.entities.iter().find(|e| e.id == id)
    }

    /// The field values of the entity with `id`, if present.
    pub fn fields_of(&self, id: NetId) -> Option<&[FieldValue]> {
        self.get(id).map(|e| e.fields.as_slice())
    }

    /// The entity ids present, in stored order.
    pub fn ids(&self) -> impl Iterator<Item = NetId> + '_ {
        self.entities.iter().map(|e| e.id)
    }
}

/// Entities per leaf of a [`CowMap`] (a leaf splits at twice this). Small leaves keep the copy a
/// changed entity costs small; the root then holds one pointer per leaf.
const COW_LEAF: usize = 16;

/// One leaf of a [`CowMap`]: entries sorted by id, never empty.
type CowLeaf<V> = Vec<(NetId, Arc<V>)>;

/// A sorted map from entity id to a shared value, copied on write (spec 120 § 8.1, R1).
///
/// Every packet the encoder sends (and every packet the decoder applies) becomes a baseline: the
/// acknowledged baseline with the packet's records applied. Stored as whole maps, 64 baselines of
/// a mostly static world were 64 copies of it (about 20 MB per client at 1,000 entities x 8
/// fields) and every encode cloned one. Here a baseline is a root of shared leaves of shared
/// values: cloning it is one reference count, and changing `k` entities copies the root's pointer
/// list once, at most `k` leaves of [`COW_LEAF`]-ish entries, and the `k` new values. Unchanged
/// leaves and values stay shared by every baseline that holds them.
#[derive(Debug)]
struct CowMap<V: ?Sized> {
    leaves: Arc<Vec<Arc<CowLeaf<V>>>>,
    len: usize,
}

impl<V: ?Sized> Clone for CowMap<V> {
    fn clone(&self) -> Self {
        Self {
            leaves: Arc::clone(&self.leaves),
            len: self.len,
        }
    }
}

impl<V: ?Sized> Default for CowMap<V> {
    fn default() -> Self {
        Self {
            leaves: Arc::new(Vec::new()),
            len: 0,
        }
    }
}

impl<V: ?Sized> CowMap<V> {
    /// A map from entries already sorted by id with no duplicates.
    fn from_sorted(entries: impl IntoIterator<Item = (NetId, Arc<V>)>) -> Self {
        let mut leaves: Vec<Arc<CowLeaf<V>>> = Vec::new();
        let mut leaf: CowLeaf<V> = Vec::with_capacity(COW_LEAF);
        let mut len = 0;
        for entry in entries {
            leaf.push(entry);
            len += 1;
            if leaf.len() == COW_LEAF {
                leaves.push(Arc::new(std::mem::replace(
                    &mut leaf,
                    Vec::with_capacity(COW_LEAF),
                )));
            }
        }
        if !leaf.is_empty() {
            leaves.push(Arc::new(leaf));
        }
        Self {
            leaves: Arc::new(leaves),
            len,
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Entries in id order.
    fn iter(&self) -> impl Iterator<Item = (NetId, &Arc<V>)> + '_ {
        self.leaves
            .iter()
            .flat_map(|leaf| leaf.iter().map(|(id, value)| (*id, value)))
    }

    /// The leaf that holds `id` or would: the last leaf whose first id is at most `id` (leaf 0
    /// for an id below every leaf).
    fn leaf_index(&self, id: NetId) -> usize {
        self.leaves
            .partition_point(|leaf| leaf.first().is_none_or(|(first, _)| *first <= id))
            .saturating_sub(1)
    }

    fn get(&self, id: NetId) -> Option<&Arc<V>> {
        let leaf = self.leaves.get(self.leaf_index(id))?;
        let at = leaf.binary_search_by_key(&id, |(held, _)| *held).ok()?;
        leaf.get(at).map(|(_, value)| value)
    }

    fn contains(&self, id: NetId) -> bool {
        self.get(id).is_some()
    }

    /// Inserts or replaces `id`. Copies the root and the one leaf it touches if they are shared.
    fn insert(&mut self, id: NetId, value: Arc<V>) {
        let index = self.leaf_index(id);
        let leaves = Arc::make_mut(&mut self.leaves);
        let Some(slot) = leaves.get_mut(index) else {
            leaves.push(Arc::new(vec![(id, value)]));
            self.len += 1;
            return;
        };
        let leaf = Arc::make_mut(slot);
        match leaf.binary_search_by_key(&id, |(held, _)| *held) {
            Ok(at) => {
                if let Some(entry) = leaf.get_mut(at) {
                    entry.1 = value;
                }
            }
            Err(at) => {
                leaf.insert(at, (id, value));
                self.len += 1;
                if leaf.len() >= 2 * COW_LEAF {
                    let tail = leaf.split_off(COW_LEAF);
                    leaves.insert(index + 1, Arc::new(tail));
                }
            }
        }
    }

    /// Removes `id`; whether it was present. An emptied leaf leaves the root.
    fn remove(&mut self, id: NetId) -> bool {
        let index = self.leaf_index(id);
        let present = self
            .leaves
            .get(index)
            .is_some_and(|leaf| leaf.binary_search_by_key(&id, |(held, _)| *held).is_ok());
        if !present {
            return false;
        }
        let leaves = Arc::make_mut(&mut self.leaves);
        let Some(slot) = leaves.get_mut(index) else {
            return false;
        };
        let leaf = Arc::make_mut(slot);
        if let Ok(at) = leaf.binary_search_by_key(&id, |(held, _)| *held) {
            leaf.remove(at);
            self.len -= 1;
        }
        if leaf.is_empty() {
            leaves.remove(index);
        }
        true
    }
}

/// A reconstructed state keyed by entity: the decoder ring buffer's element.
type StateMap = CowMap<[FieldValue]>;

/// Wire keys keyed by entity: the encoder ring buffer's element. The encoder stores the exact
/// keys it sent rather than the values, so change detection compares what the client actually
/// holds (re-deriving a key from a quantised value could drift by one step for a quaternion
/// whose largest component is a near tie).
type KeyMap = CowMap<[WireKey]>;

/// Whether sequence `a` is newer than `b` in serial-number arithmetic (RFC 1982): sequences wrap,
/// so "newer" means less than half the sequence space ahead.
fn seq_newer(a: Seq, b: Seq) -> bool {
    (a.0.wrapping_sub(b.0) as i32) > 0
}

/// Which entities [`SnapshotEncoder::encode_selecting`] holds back or always sends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecordSelection<'a> {
    /// Entities whose updates stay at the acknowledged state (no record).
    pub defer: &'a [NetId],
    /// Entities that get an update record even when unchanged against the baseline.
    pub force: &'a [NetId],
}

/// What one encode produced, beyond the packet bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodeReport {
    /// The sequence number assigned to the packet.
    pub seq: Seq,
    /// The tick the packet carries.
    pub tick: NetTick,
    /// The baseline the packet was encoded against (`None` for a full snapshot).
    pub baseline: Option<Seq>,
    /// Whether the packet is a full snapshot (no baseline).
    pub is_full: bool,
    /// Whether every record fit the budget. An incomplete packet is still stored as a baseline,
    /// holding the acknowledged baseline with only the included records applied.
    pub complete: bool,
    /// Entities whose records were packed, sorted by id.
    pub included: Vec<NetId>,
    /// Entities whose records were dropped for budget, sorted by id. Their next encode still
    /// diffs against the acknowledged baseline, so nothing is lost, only deferred.
    pub omitted: Vec<NetId>,
    /// The packet's length in bytes (never above the budget).
    pub bytes: usize,
}

/// One record waiting to be packed: its finished bits plus its packing metadata.
#[derive(Debug)]
struct PendingRecord {
    id: NetId,
    priority: u32,
    /// Whether the record removes the entity (otherwise it adds or updates it, leaving the client
    /// holding the entity's current keys).
    remove: bool,
    /// The entity's index in the encoded world (unused for a remove): where its keys sit in the
    /// encoder's key buffer.
    index: usize,
    /// Whether the record fit the budget.
    included: bool,
    writer: BitWriter,
}

impl PendingRecord {
    fn bits(&self) -> usize {
        self.writer.bit_len()
    }
}

/// A record writer sized for the schema's widest record, so building a record allocates once
/// (spec 120 § 8.1: encode allocations follow the records sent, not the world).
fn record_writer(schema: &SnapshotSchema) -> BitWriter {
    // kind (2) + id varint (up to 40) + one mask bit and the widest value per field.
    let bits: u32 = 42
        + schema
            .fields
            .iter()
            .map(|def| 1 + def.kind.wire_bits().unwrap_or(40))
            .sum::<u32>();
    BitWriter::with_capacity(bits.div_ceil(8) as usize)
}

/// Builds an `add` record carrying every field.
fn add_record(
    schema: &SnapshotSchema,
    id: NetId,
    priority: u32,
    index: usize,
    keys: &[WireKey],
) -> PendingRecord {
    let mut writer = record_writer(schema);
    writer.write_bits(RECORD_ADD, 2);
    writer.write_varint(u64::from(id.0));
    for (def, key) in schema.fields.iter().zip(keys.iter()) {
        write_key(&def.kind, *key, &mut writer);
    }
    PendingRecord {
        id,
        priority,
        remove: false,
        index,
        included: false,
        writer,
    }
}

/// Builds an `update` record: a change mask then the changed fields. `None` when nothing
/// changed at the wire level.
fn update_record(
    schema: &SnapshotSchema,
    id: NetId,
    priority: u32,
    index: usize,
    baseline: &[WireKey],
    current: &[WireKey],
) -> Option<PendingRecord> {
    if baseline == current {
        return None;
    }
    let mut writer = record_writer(schema);
    writer.write_bits(RECORD_UPDATE, 2);
    writer.write_varint(u64::from(id.0));
    for (base, cur) in baseline.iter().zip(current.iter()) {
        writer.write_bool(base != cur);
    }
    for (def, (base, cur)) in schema.fields.iter().zip(baseline.iter().zip(current.iter())) {
        if base != cur {
            write_key(&def.kind, *cur, &mut writer);
        }
    }
    Some(PendingRecord {
        id,
        priority,
        remove: false,
        index,
        included: false,
        writer,
    })
}

/// Builds an `update` record for an entity whose wire state equals its baseline's: the change mask
/// names only the schema's narrowest field, re-sent at its current value (an update must change at
/// least one field on the wire, spec 120 § 8.6).
fn forced_record(
    schema: &SnapshotSchema,
    id: NetId,
    priority: u32,
    index: usize,
    current: &[WireKey],
) -> PendingRecord {
    let narrowest = schema
        .fields
        .iter()
        .enumerate()
        .min_by_key(|(_, def)| def.kind.wire_bits().unwrap_or(u32::MAX))
        .map_or(0, |(at, _)| at);
    let mut writer = record_writer(schema);
    writer.write_bits(RECORD_UPDATE, 2);
    writer.write_varint(u64::from(id.0));
    for at in 0..current.len() {
        writer.write_bool(at == narrowest);
    }
    if let (Some(def), Some(key)) = (schema.fields.get(narrowest), current.get(narrowest)) {
        write_key(&def.kind, *key, &mut writer);
    }
    PendingRecord {
        id,
        priority,
        remove: false,
        index,
        included: false,
        writer,
    }
}

/// Builds a `remove` record. Removals pack ahead of everything: a stale entity left on the
/// client is worse than a deferred update.
fn remove_record(id: NetId) -> PendingRecord {
    let mut writer = BitWriter::with_capacity(6);
    writer.write_bits(RECORD_REMOVE, 2);
    writer.write_varint(u64::from(id.0));
    PendingRecord {
        id,
        priority: u32::MAX,
        remove: true,
        index: 0,
        included: false,
        writer,
    }
}

/// The sending side. Holds the schema and a ring buffer, keyed by sequence number, of the state
/// each packet it sent leaves the client holding, so a later encode can delta against whichever
/// one the client last acknowledged.
#[derive(Debug)]
pub struct SnapshotEncoder {
    schema: SnapshotSchema,
    /// Baselines, oldest first. Each shares its unchanged leaves and values with the others.
    ring: VecDeque<(Seq, KeyMap)>,
    capacity: usize,
    next_seq: u32,
    last_report: Option<EncodeReport>,
    /// Reused per encode: every entity's wire keys, `field_count` per entity in input order.
    keys: Vec<WireKey>,
    /// Reused per encode: entity indices sorted by id.
    order: Vec<usize>,
    /// Reused per encode: the records of one packet.
    records: Vec<PendingRecord>,
}

impl SnapshotEncoder {
    /// An encoder holding [`DEFAULT_BASELINE_CAPACITY`] baselines.
    pub fn new(schema: SnapshotSchema) -> Self {
        Self::with_capacity(schema, DEFAULT_BASELINE_CAPACITY)
    }

    /// An encoder holding at most `capacity` baselines (clamped to at least 1). Size it to the
    /// worst round trip in send ticks: an ack older than the ring costs one full snapshot.
    pub fn with_capacity(schema: SnapshotSchema, capacity: usize) -> Self {
        Self {
            schema,
            ring: VecDeque::new(),
            capacity: capacity.max(1),
            next_seq: 1,
            last_report: None,
            keys: Vec::new(),
            order: Vec::new(),
            records: Vec::new(),
        }
    }

    /// The schema this encoder writes.
    pub fn schema(&self) -> &SnapshotSchema {
        &self.schema
    }

    /// How many baselines the ring may hold.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// How many baselines the ring currently holds.
    pub fn baseline_count(&self) -> usize {
        self.ring.len()
    }

    /// Whether `seq` is still available as a delta baseline.
    pub fn has_baseline(&self, seq: Seq) -> bool {
        self.ring.iter().any(|(held, _)| *held == seq)
    }

    /// The sequence the next successful encode will assign.
    pub fn next_seq(&self) -> Seq {
        Seq(self.next_seq)
    }

    /// The report of the most recent successful encode, if any.
    pub fn last_report(&self) -> Option<&EncodeReport> {
        self.last_report.as_ref()
    }

    /// Encodes `world` at `tick` into one packet of at most `budget_bytes`.
    ///
    /// `ack` is the newest sequence the client has acknowledged receiving. When it names a
    /// baseline still in the ring, the packet is a delta against it; otherwise (no ack yet, or a
    /// baseline that fell out of the ring) it is a full snapshot. Records pack by descending priority; those that do not fit are listed in the
    /// report obtainable from [`SnapshotEncoder::last_report`] or
    /// [`SnapshotEncoder::encode_with_report`].
    pub fn encode(
        &mut self,
        tick: NetTick,
        world: &WorldSnapshot,
        ack: Option<Seq>,
        budget_bytes: usize,
    ) -> Result<Vec<u8>, SnapshotError> {
        Ok(self.encode_with_report(tick, world, ack, budget_bytes)?.0)
    }

    /// [`SnapshotEncoder::encode`], also returning the [`EncodeReport`].
    pub fn encode_with_report(
        &mut self,
        tick: NetTick,
        world: &WorldSnapshot,
        ack: Option<Seq>,
        budget_bytes: usize,
    ) -> Result<(Vec<u8>, EncodeReport), SnapshotError> {
        self.encode_inner(tick, world, ack, budget_bytes, RecordSelection::default())
    }

    /// [`SnapshotEncoder::encode_with_report`] with a per-entity record selection (spec 120 § 8.3
    /// rate tiers, § 8.6 replication outside the ECS).
    ///
    /// An entity in `selection.defer` that the acknowledged baseline holds gets no update record:
    /// the packet's baseline keeps it at its acknowledged state, exactly as if a budget had omitted
    /// it, but it counts as neither included nor omitted and does not make the packet incomplete.
    /// An entity in `selection.force` that the baseline holds always gets an update record, even
    /// when its wire state equals the baseline's (then the record re-sends the schema's narrowest
    /// field, so the receiver sees the entity as updated at this tick). An entity the baseline
    /// lacks is added regardless (nothing withholds a spawn), and an entity absent from `world` is
    /// removed regardless. A full packet (no usable baseline) defers nothing. The lists may be in
    /// any order; ids not in `world` are ignored; `defer` wins over `force`.
    pub fn encode_selecting(
        &mut self,
        tick: NetTick,
        world: &WorldSnapshot,
        ack: Option<Seq>,
        budget_bytes: usize,
        selection: RecordSelection<'_>,
    ) -> Result<(Vec<u8>, EncodeReport), SnapshotError> {
        let sorted = |ids: &[NetId]| -> Option<Vec<NetId>> {
            (!ids.is_sorted()).then(|| {
                let mut v = ids.to_vec();
                v.sort_unstable();
                v
            })
        };
        let (defer, force) = (sorted(selection.defer), sorted(selection.force));
        self.encode_inner(
            tick,
            world,
            ack,
            budget_bytes,
            RecordSelection {
                defer: defer.as_deref().unwrap_or(selection.defer),
                force: force.as_deref().unwrap_or(selection.force),
            },
        )
    }

    /// The encode of [`SnapshotEncoder::encode_selecting`]; both lists are sorted.
    fn encode_inner(
        &mut self,
        tick: NetTick,
        world: &WorldSnapshot,
        ack: Option<Seq>,
        budget_bytes: usize,
        selection: RecordSelection<'_>,
    ) -> Result<(Vec<u8>, EncodeReport), SnapshotError> {
        let _span = tracing::trace_span!("regia_net::snapshot_encode").entered();
        if world.entities.len() > MAX_ENTITIES {
            return Err(SnapshotError::TooManyEntities {
                count: world.entities.len(),
                max: MAX_ENTITIES,
            });
        }
        let fields = self.schema.field_count();
        // Validate and quantise into one reused buffer (no allocation per entity), then sort the
        // entity indices by id: from here on, nothing depends on input order.
        self.keys.clear();
        self.keys.reserve(world.entities.len() * fields);
        for entity in &world.entities {
            self.schema.entity_keys_into(entity, &mut self.keys)?;
        }
        self.order.clear();
        self.order.extend(0..world.entities.len());
        let entities = &world.entities;
        self.order
            .sort_unstable_by_key(|&index| entities.get(index).map_or(NetId(u32::MAX), |e| e.id));
        for pair in self.order.windows(2) {
            if let [a, b] = *pair
                && let (Some(first), Some(second)) = (entities.get(a), entities.get(b))
                && first.id == second.id
            {
                return Err(SnapshotError::DuplicateEntity(first.id));
            }
        }
        let keys_of = |index: usize| self.keys.get(index * fields..(index + 1) * fields).unwrap_or(&[]);

        // The acknowledged baseline, if the ring still holds it. The ring stores the exact wire
        // keys that were sent for it, so the diff below compares like with like. Cloning a
        // baseline is one reference count: its leaves stay shared.
        let baseline: Option<(Seq, KeyMap)> = ack.and_then(|wanted| {
            self.ring
                .iter()
                .find(|(held, _)| *held == wanted)
                .map(|(_, keys)| (wanted, keys.clone()))
        });
        let is_full = baseline.is_none();

        // A merge of the baseline (id order) with the world (id order).
        let mut records = std::mem::take(&mut self.records);
        records.clear();
        let empty = KeyMap::default();
        let base = baseline.as_ref().map_or(&empty, |(_, keys)| keys);
        let mut held = base.iter().peekable();
        for &index in &self.order {
            let Some(entity) = entities.get(index) else {
                continue;
            };
            while let Some((id, _)) = held.next_if(|(id, _)| *id < entity.id) {
                records.push(remove_record(id));
            }
            let keys = keys_of(index);
            match held.next_if(|(id, _)| *id == entity.id) {
                None => records.push(add_record(&self.schema, entity.id, entity.priority, index, keys)),
                // A deferred update stays at the baseline's state (spec 120 § 8.3).
                Some(_) if selection.defer.binary_search(&entity.id).is_ok() => {}
                Some((_, base_keys)) => {
                    if let Some(record) =
                        update_record(&self.schema, entity.id, entity.priority, index, base_keys, keys)
                    {
                        records.push(record);
                    } else if selection.force.binary_search(&entity.id).is_ok() {
                        records.push(forced_record(
                            &self.schema,
                            entity.id,
                            entity.priority,
                            index,
                            keys,
                        ));
                    }
                }
            }
        }
        for (id, _) in held {
            records.push(remove_record(id));
        }
        // Pack the most important work first; id order settles ties (ids are unique, so the
        // unstable sort is deterministic and needs no scratch buffer).
        records.sort_unstable_by(|a, b| b.priority.cmp(&a.priority).then_with(|| a.id.cmp(&b.id)));

        let seq = Seq(self.next_seq);
        let baseline_seq = baseline.as_ref().map(|(held, _)| *held);
        // The count varint is reserved at its largest possible width (the record total), so the
        // budget arithmetic below never underestimates the header.
        let header_bits = 32
            + 8
            + varint_bits(u64::from(seq.0))
            + varint_bits(u64::from(tick.0))
            + varint_bits(baseline_seq.map_or(0, |s| u64::from(s.0)))
            + varint_bits(records.len() as u64);
        let minimum = OUTER_HEADER_BYTES + header_bits.div_ceil(8);
        if budget_bytes < minimum {
            self.records = records;
            return Err(SnapshotError::BudgetTooSmall {
                budget: budget_bytes,
                minimum,
            });
        }

        let mut omitted: Vec<NetId> = Vec::new();
        let mut included_count = 0usize;
        let mut used_bits = header_bits;
        for record in &mut records {
            let total = OUTER_HEADER_BYTES + (used_bits + record.bits()).div_ceil(8);
            if total <= budget_bytes {
                used_bits += record.bits();
                record.included = true;
                included_count += 1;
            } else {
                omitted.push(record.id);
            }
        }
        let complete = omitted.is_empty();

        let mut body = BitWriter::with_capacity(used_bits.div_ceil(8) + 2);
        body.write_bits(u64::from(self.schema.layout_hash()), 32);
        let mut flags = 0u64;
        if is_full {
            flags |= FLAG_FULL;
        }
        if complete {
            flags |= FLAG_COMPLETE;
        }
        body.write_bits(flags, 8);
        body.write_varint(u64::from(seq.0));
        body.write_varint(u64::from(tick.0));
        body.write_varint(baseline_seq.map_or(0, |s| u64::from(s.0)));
        body.write_varint(included_count as u64);
        let mut included_ids: Vec<NetId> = Vec::with_capacity(included_count);
        for record in records.iter().filter(|record| record.included) {
            included_ids.push(record.id);
            body.append(&record.writer);
        }
        let body = body.finish();

        let total_len = OUTER_HEADER_BYTES + body.len();
        let Ok(total_prefix) = u32::try_from(total_len) else {
            self.records = records;
            return Err(SnapshotError::PacketTooLarge { len: total_len });
        };
        let mut packet = Vec::with_capacity(total_len);
        packet.push(PACKET_MAGIC);
        packet.push(PACKET_VERSION);
        packet.extend_from_slice(&total_prefix.to_le_bytes());
        packet.extend_from_slice(&crc32_ieee(&body).to_le_bytes());
        packet.extend_from_slice(&body);

        // Success is certain: commit the sequence and the new baseline. The baseline is what the
        // client holds after applying this packet: the acknowledged state (empty for a full
        // snapshot) with the included records applied. Omitted records leave the client's copy
        // at the baseline, so the next encode diffs them again. Storing incomplete packets too
        // is what makes a budgeted stream converge: every acknowledged packet is progress.
        // Copy-on-write: only the leaves the included records touch are copied.
        self.next_seq = match self.next_seq.wrapping_add(1) {
            0 => 1,
            next => next,
        };
        let mut state: KeyMap = baseline.map(|(_, keys)| keys).unwrap_or_default();
        for record in records.iter().filter(|record| record.included) {
            if record.remove {
                state.remove(record.id);
            } else {
                state.insert(record.id, Arc::from(keys_of(record.index)));
            }
        }
        records.clear();
        self.records = records;
        self.ring.push_back((seq, state));
        while self.ring.len() > self.capacity {
            self.ring.pop_front();
        }
        included_ids.sort_unstable();
        omitted.sort_unstable();
        let report = EncodeReport {
            seq,
            tick,
            baseline: baseline_seq,
            is_full,
            complete,
            included: included_ids,
            omitted,
            bytes: packet.len(),
        };
        self.last_report = Some(report.clone());
        Ok((packet, report))
    }
}

/// What one decode produced: the reconstructed world plus what the packet changed.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedSnapshot {
    /// The packet's sequence number.
    pub seq: Seq,
    /// The packet's tick.
    pub tick: NetTick,
    /// The baseline the packet was encoded against (`None` for a full snapshot).
    pub baseline: Option<Seq>,
    /// Whether the packet was a full snapshot.
    pub is_full: bool,
    /// Whether the packet carried every record (see [`EncodeReport::complete`]). Complete or not,
    /// the decoded packet becomes a baseline; acknowledge [`SnapshotDecoder::latest_baseline`].
    pub complete: bool,
    /// The reconstructed world after applying the packet, entities sorted by id and field values
    /// quantised exactly as sent. Priorities are not on the wire and read 0.
    pub snapshot: WorldSnapshot,
    /// Entities the packet added, sorted by id.
    pub added: Vec<NetId>,
    /// Entities the packet updated, sorted by id.
    pub updated: Vec<NetId>,
    /// Entities the packet removed, sorted by id.
    pub removed: Vec<NetId>,
}

/// Reads a varint that must fit a `u32`.
fn read_varint_u32(r: &mut BitReader<'_>, what: &'static str) -> Result<u32, SnapshotError> {
    let raw = r.read_varint()?;
    u32::try_from(raw).map_err(|_| SnapshotError::InvalidHeader(what))
}

/// A little-endian `u32` out of a packet's outer header, or 0 when the slice is too short
/// (callers have already checked the length; 0 keeps this panic-free regardless).
fn header_u32(packet: &[u8], at: usize) -> u32 {
    let bytes: [u8; 4] = packet
        .get(at..at + 4)
        .and_then(|slice| slice.try_into().ok())
        .unwrap_or([0; 4]);
    u32::from_le_bytes(bytes)
}

/// The receiving side. Holds the schema, the newest reconstructed snapshot, and a ring buffer of
/// the snapshots it has reconstructed, keyed by sequence number, so a delta naming any of them as
/// its baseline can be applied.
///
/// Packets may arrive late, twice or out of order (spec 120 § 8.1, R1). A packet newer than the
/// newest decoded one moves [`SnapshotDecoder::current`] forward; an older (or repeated) one only
/// becomes a baseline a later delta may name: the shown world never moves backward. A full packet
/// that the sender's byte budget cut short (sent after the client's acknowledged baseline fell out
/// of the sender's ring) is merged into the shown world instead of replacing it: entities it did
/// not carry stay shown, at their last known state, until a later packet carries them or a
/// complete packet settles the world ([`SnapshotDecoder::is_rebasing`]).
#[derive(Debug)]
pub struct SnapshotDecoder {
    schema: SnapshotSchema,
    /// Baselines in ascending sequence order (serial arithmetic). Each shares its unchanged
    /// leaves and values with the others.
    ring: VecDeque<(Seq, StateMap)>,
    capacity: usize,
    /// The shown world as a map: the newest packet's state, plus, while re-basing, the entities
    /// it lacks that were shown before.
    shown: StateMap,
    /// Whether `shown` holds entities the newest packet's state lacks.
    rebasing: bool,
    current: Option<WorldSnapshot>,
    last_seq: Option<Seq>,
}

impl SnapshotDecoder {
    /// A decoder holding [`DEFAULT_BASELINE_CAPACITY`] baselines.
    pub fn new(schema: SnapshotSchema) -> Self {
        Self::with_capacity(schema, DEFAULT_BASELINE_CAPACITY)
    }

    /// A decoder holding at most `capacity` baselines (clamped to at least 1). Match or exceed
    /// the encoder's capacity: a delta against a baseline this decoder already evicted fails
    /// with [`SnapshotError::BaselineMissing`] and the sender must re-base.
    pub fn with_capacity(schema: SnapshotSchema, capacity: usize) -> Self {
        Self {
            schema,
            ring: VecDeque::new(),
            capacity: capacity.max(1),
            shown: StateMap::default(),
            rebasing: false,
            current: None,
            last_seq: None,
        }
    }

    /// Forgets every baseline and the shown world, as a new decoder would. Call it when the
    /// sender's stream restarts (a reconnect to a new server process): its sequences start again
    /// at 1, and without a reset only a full packet more than [`SnapshotDecoder::capacity`]
    /// sequences behind the newest decoded one is taken as a restart.
    pub fn reset(&mut self) {
        self.ring.clear();
        self.shown = StateMap::default();
        self.rebasing = false;
        self.current = None;
        self.last_seq = None;
    }

    /// Whether the shown world still holds entities the newest packet did not carry: a full
    /// packet cut short by the sender's budget arrived (after the client's acknowledged baseline
    /// fell out of the sender's ring), and the stream has not yet re-sent every entity. Ends at
    /// the first complete packet. While it lasts, an entity removed on the sender may stay shown.
    pub fn is_rebasing(&self) -> bool {
        self.rebasing
    }

    /// The schema this decoder reads.
    pub fn schema(&self) -> &SnapshotSchema {
        &self.schema
    }

    /// How many baselines the ring may hold.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// How many baselines the ring currently holds.
    pub fn baseline_count(&self) -> usize {
        self.ring.len()
    }

    /// Whether `seq` is still held as a baseline.
    pub fn has_baseline(&self, seq: Seq) -> bool {
        self.ring.iter().any(|(held, _)| *held == seq)
    }

    /// The newest sequence held as a baseline: the value to acknowledge to the sender. A late
    /// packet never lowers it.
    pub fn latest_baseline(&self) -> Option<Seq> {
        self.ring.back().map(|(held, _)| *held)
    }

    /// The sequence of the newest successfully decoded packet, complete or not (a late packet
    /// decoded after it does not change it).
    pub fn last_seq(&self) -> Option<Seq> {
        self.last_seq
    }

    /// The shown world: the newest reconstructed snapshot (merged with not yet re-sent entities
    /// while [`SnapshotDecoder::is_rebasing`]), if any packet has decoded.
    pub fn current(&self) -> Option<&WorldSnapshot> {
        self.current.as_ref()
    }

    /// Decodes one packet and applies it.
    ///
    /// A complete full packet replaces the world with its records; a full packet cut short by the
    /// budget is merged into the shown world (see [`SnapshotDecoder::is_rebasing`]). A delta
    /// applies its adds, updates and removes on top of the named baseline (which is why omitted
    /// entities keep their baseline state when a packet is incomplete). Known limit (ADR-10051
    /// audit): that also holds for an entity a newer packet added. While its acknowledgment is
    /// on the way the next delta is still against the older baseline, and if the budget omits the
    /// entity's record there, [`SnapshotDecoder::current`] loses the entity until a later packet
    /// carries it again. The packet cannot say whether the sender omitted the record or removed
    /// the entity, so the decoder does not guess; `external_replication`'s interpolator keeps its
    /// tracks through such a gap.
    ///
    /// A packet not newer than [`SnapshotDecoder::last_seq`] (reordered or duplicated) is stored
    /// as a baseline only: the returned [`DecodedSnapshot`] carries the packet's own `seq`, `tick`
    /// and `baseline`, but its `snapshot` is the unchanged [`SnapshotDecoder::current`] and its
    /// `added`, `updated` and `removed` lists are empty. Tell the two apart by comparing `seq`
    /// with [`SnapshotDecoder::last_seq`] after the call. A full packet more than
    /// [`SnapshotDecoder::capacity`] sequences behind is taken as a restarted stream and resets
    /// the decoder first.
    ///
    /// Any error (truncation, corruption, schema mismatch, a missing baseline) leaves the decoder
    /// exactly as it was.
    pub fn decode(&mut self, packet: &[u8]) -> Result<DecodedSnapshot, SnapshotError> {
        let _span = tracing::trace_span!("regia_net::snapshot_decode").entered();
        if packet.len() < OUTER_HEADER_BYTES {
            return Err(SnapshotError::TooShort {
                len: packet.len(),
                minimum: OUTER_HEADER_BYTES,
            });
        }
        let magic = packet.first().copied().unwrap_or(0);
        if magic != PACKET_MAGIC {
            return Err(SnapshotError::BadMagic { got: magic });
        }
        let version = packet.get(1).copied().unwrap_or(0);
        if version != PACKET_VERSION {
            return Err(SnapshotError::UnsupportedVersion { got: version });
        }
        let declared = header_u32(packet, 2) as usize;
        if declared != packet.len() {
            return Err(SnapshotError::LengthMismatch {
                declared,
                actual: packet.len(),
            });
        }
        let expected_crc = header_u32(packet, 6);
        let body = packet.get(OUTER_HEADER_BYTES..).unwrap_or(&[]);
        let computed_crc = crc32_ieee(body);
        if computed_crc != expected_crc {
            return Err(SnapshotError::CrcMismatch {
                expected: expected_crc,
                computed: computed_crc,
            });
        }

        let mut reader = BitReader::new(body);
        let packet_hash = reader.read_bits(32)? as u32;
        if packet_hash != self.schema.layout_hash() {
            return Err(SnapshotError::SchemaMismatch {
                expected: self.schema.layout_hash(),
                got: packet_hash,
            });
        }
        let flags = reader.read_bits(8)?;
        if flags & !(FLAG_FULL | FLAG_COMPLETE) != 0 {
            return Err(SnapshotError::InvalidHeader("reserved flag bits set"));
        }
        let is_full = flags & FLAG_FULL != 0;
        let complete = flags & FLAG_COMPLETE != 0;
        let seq_raw = read_varint_u32(&mut reader, "sequence out of range")?;
        if seq_raw == 0 {
            return Err(SnapshotError::InvalidHeader("sequence is zero"));
        }
        let seq = Seq(seq_raw);
        let tick = NetTick(read_varint_u32(&mut reader, "tick out of range")?);
        let baseline_raw = read_varint_u32(&mut reader, "baseline out of range")?;
        let baseline = match baseline_raw {
            0 => None,
            raw => Some(Seq(raw)),
        };
        if is_full != baseline.is_none() {
            return Err(SnapshotError::InvalidHeader("full flag and baseline disagree"));
        }
        if baseline == Some(seq) {
            return Err(SnapshotError::InvalidHeader("packet is its own baseline"));
        }
        let count = reader.read_len(MAX_ENTITIES)?;

        // Everything from here works on a scratch state: an error anywhere below returns with
        // the decoder untouched.
        // Cloning a baseline is one reference count; the records below copy only what they touch.
        let mut state: StateMap = match baseline {
            None => StateMap::default(),
            Some(wanted) => self
                .ring
                .iter()
                .find(|(held, _)| *held == wanted)
                .map(|(_, held)| held.clone())
                .ok_or(SnapshotError::BaselineMissing { seq: wanted })?,
        };

        let mut added: Vec<NetId> = Vec::new();
        let mut updated: Vec<NetId> = Vec::new();
        let mut removed: Vec<NetId> = Vec::new();
        let mut seen: BTreeSet<NetId> = BTreeSet::new();
        for _ in 0..count {
            let kind = reader.read_bits(2)?;
            let id = NetId(read_varint_u32(&mut reader, "entity id out of range")?);
            if !seen.insert(id) {
                return Err(SnapshotError::InvalidRecord("entity appears twice in one packet"));
            }
            match kind {
                RECORD_ADD => {
                    if state.contains(id) {
                        return Err(SnapshotError::InvalidRecord(
                            "add for an entity the baseline already holds",
                        ));
                    }
                    let mut values = Vec::with_capacity(self.schema.field_count());
                    for def in &self.schema.fields {
                        values.push(value_of(&def.kind, read_key(&def.kind, &mut reader)?));
                    }
                    state.insert(id, Arc::from(values));
                    // The encoder never holds more than MAX_ENTITIES; a peer's deltas that each
                    // add up to the per-packet limit grew the state (and the 64 kept baselines)
                    // without bound (ADR-5870).
                    if state.len() > MAX_ENTITIES {
                        return Err(SnapshotError::TooManyEntities {
                            count: state.len(),
                            max: MAX_ENTITIES,
                        });
                    }
                    added.push(id);
                }
                RECORD_UPDATE => {
                    let Some(mut values) = state.get(id).map(|held| held.to_vec()) else {
                        return Err(SnapshotError::InvalidRecord(
                            "update for an entity the baseline does not hold",
                        ));
                    };
                    if values.len() != self.schema.field_count() {
                        return Err(SnapshotError::InvalidRecord(
                            "stored baseline field count differs from the schema",
                        ));
                    }
                    let mut mask: Vec<bool> = Vec::with_capacity(self.schema.field_count());
                    for _ in 0..self.schema.field_count() {
                        mask.push(reader.read_bool()?);
                    }
                    if !mask.iter().any(|changed| *changed) {
                        return Err(SnapshotError::InvalidRecord("update changes nothing"));
                    }
                    for (def, (changed, slot)) in
                        self.schema.fields.iter().zip(mask.iter().zip(values.iter_mut()))
                    {
                        if *changed {
                            *slot = value_of(&def.kind, read_key(&def.kind, &mut reader)?);
                        }
                    }
                    state.insert(id, Arc::from(values));
                    updated.push(id);
                }
                RECORD_REMOVE => {
                    if !state.remove(id) {
                        return Err(SnapshotError::InvalidRecord(
                            "remove for an entity the baseline does not hold",
                        ));
                    }
                    removed.push(id);
                }
                _ => return Err(SnapshotError::InvalidRecord("reserved record kind")),
            }
        }

        let remaining = reader.remaining_bits();
        if remaining >= 8 {
            return Err(SnapshotError::TrailingBits { bits: remaining });
        }
        if remaining > 0 && reader.read_bits(remaining as u32)? != 0 {
            return Err(SnapshotError::InvalidHeader("non-zero padding bits"));
        }

        let restart = match self.last_seq {
            None => false,
            Some(last) => {
                is_full && !seq_newer(seq, last) && last.0.wrapping_sub(seq.0) as usize > self.capacity
            }
        };
        let newer = restart || self.last_seq.is_none_or(|last| seq_newer(seq, last));
        if !newer {
            // Late or repeated (R1): keep it as a baseline a later delta may name, in sequence
            // order (a repeat replaces its own entry), but never move the shown world back.
            let at = self.ring.iter().position(|(held, _)| !seq_newer(seq, *held));
            match at {
                Some(at) if self.ring.get(at).is_some_and(|(held, _)| *held == seq) => {
                    if let Some(entry) = self.ring.get_mut(at) {
                        entry.1 = state;
                    }
                }
                Some(at) => self.ring.insert(at, (seq, state)),
                None => self.ring.push_back((seq, state)),
            }
            while self.ring.len() > self.capacity {
                self.ring.pop_front();
            }
            return Ok(DecodedSnapshot {
                seq,
                tick,
                baseline,
                is_full,
                complete,
                snapshot: self.current.clone().unwrap_or_default(),
                added: Vec::new(),
                updated: Vec::new(),
                removed: Vec::new(),
            });
        }
        if restart {
            // A restarted stream is a new world: what the old stream showed is not merged into
            // its first packets (the documented reset; ADR-10051 found the shown world kept).
            self.ring.clear();
            self.shown = StateMap::default();
            self.rebasing = false;
        }

        // The shown world. A complete packet is the sender's whole world: show exactly it. A full
        // packet cut short by the budget (the sender lost the client's baseline) or any packet
        // while re-basing is merged: what it lacks stays shown at its last known state, except
        // what it removed (R1: a partial full never wipes the world).
        removed.sort_unstable();
        let merge = !complete && (is_full || self.rebasing) && !self.shown.is_empty();
        let shown = if merge {
            let mut fresh = state.iter().peekable();
            let mut old = self.shown.iter().peekable();
            let mut merged: Vec<(NetId, Arc<[FieldValue]>)> =
                Vec::with_capacity(self.shown.len().max(state.len()));
            loop {
                match (fresh.peek(), old.peek()) {
                    (Some((a, _)), Some((b, _))) if a == b => {
                        if let Some((id, value)) = fresh.next() {
                            merged.push((id, Arc::clone(value)));
                        }
                        old.next();
                    }
                    (Some((a, _)), Some((b, _))) if a < b => {
                        if let Some((id, value)) = fresh.next() {
                            merged.push((id, Arc::clone(value)));
                        }
                    }
                    (_, Some((b, _))) => {
                        let b = *b;
                        if let Some((id, value)) = old.next()
                            && removed.binary_search(&b).is_err()
                        {
                            merged.push((id, Arc::clone(value)));
                        }
                    }
                    (Some(_), None) => {
                        if let Some((id, value)) = fresh.next() {
                            merged.push((id, Arc::clone(value)));
                        }
                    }
                    (None, None) => break,
                }
            }
            StateMap::from_sorted(merged)
        } else {
            state.clone()
        };
        self.rebasing = shown.len() != state.len();
        let snapshot = WorldSnapshot {
            entities: shown
                .iter()
                .map(|(id, values)| EntitySnapshot {
                    id,
                    priority: 0,
                    fields: values.to_vec(),
                })
                .collect(),
        };

        // Commit: the reconstructed world becomes current and a baseline future deltas may name.
        // Incomplete packets are baselines too: the encoder stored the same state (the baseline
        // with exactly the records this packet carried).
        self.ring.retain(|(held, _)| *held != seq);
        self.ring.push_back((seq, state));
        while self.ring.len() > self.capacity {
            self.ring.pop_front();
        }
        self.shown = shown;
        self.current = Some(snapshot.clone());
        self.last_seq = Some(seq);
        added.sort_unstable();
        updated.sort_unstable();
        removed.sort_unstable();
        Ok(DecodedSnapshot {
            seq,
            tick,
            baseline,
            is_full,
            complete,
            snapshot,
            added,
            updated,
            removed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Field indices in [`test_schema`].
    const SCORE: usize = 3;
    /// Field indices in [`test_schema`].
    const HEALTH: usize = 5;
    /// Field indices in [`test_schema`].
    const POS: usize = 6;
    /// Field indices in [`test_schema`].
    const ROT: usize = 7;

    /// A deterministic xorshift64* stream, so every seed below is reproducible.
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed.wrapping_add(0x9e37_79b9_7f4a_7c15) | 1)
        }

        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn below(&mut self, n: u64) -> u64 {
            if n == 0 { 0 } else { self.next_u64() % n }
        }

        fn f32_range(&mut self, lo: f32, hi: f32) -> f32 {
            let t = (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32;
            lo + t * (hi - lo)
        }

        fn quat(&mut self) -> Quat {
            let axis = Vec3::new(
                self.f32_range(-1.0, 1.0),
                self.f32_range(-1.0, 1.0),
                self.f32_range(-1.0, 1.0),
            );
            let axis = if axis.length_squared() > 1e-6 {
                axis.normalize()
            } else {
                Vec3::Y
            };
            let pi = std::f32::consts::PI;
            Quat::from_axis_angle(axis, self.f32_range(-pi, pi))
        }
    }

    /// Sec review (ADR-5870 § 6): the decoder capped records per packet only, so deltas from two
    /// encoders (or a hostile peer) that each add `MAX_ENTITIES` ids on the previous sequence
    /// grew the reconstructed state, and the 64 kept baselines, without bound.
    #[test]
    fn deltas_cannot_grow_the_state_past_max_entities() {
        let schema = SnapshotSchema::new(vec![FieldDef::new("alive", FieldKind::Bool)]).expect("schema");
        let world = |ids: std::ops::Range<u32>| {
            WorldSnapshot::new(
                ids.map(|id| EntitySnapshot::new(NetId(id), vec![FieldValue::Bool(true)]))
                    .collect(),
            )
        };
        let half = MAX_ENTITIES as u32 / 2;
        let budget = 1 << 20;
        // Encoder one: a full world of half the limit (seq 1).
        let mut one = SnapshotEncoder::new(schema.clone());
        let full = one
            .encode(NetTick(1), &world(0..half), None, budget)
            .expect("full");
        // Encoder two: an empty full (seq 1), then a delta adding the limit's worth of other ids
        // on seq 1: each packet is valid on its own.
        let mut two = SnapshotEncoder::new(schema.clone());
        two.encode(NetTick(1), &WorldSnapshot::empty(), None, budget)
            .expect("empty full");
        let delta = two
            .encode(
                NetTick(2),
                &world(half..half + MAX_ENTITIES as u32),
                Some(Seq(1)),
                budget,
            )
            .expect("delta");
        let mut dec = SnapshotDecoder::new(schema);
        dec.decode(&full).expect("the full decodes");
        match dec.decode(&delta) {
            Err(SnapshotError::TooManyEntities { count, max }) => {
                assert_eq!(max, MAX_ENTITIES);
                assert!(count > MAX_ENTITIES);
            }
            other => panic!(
                "a delta past the limit must be refused, got {:?} entities",
                other.map(|d| d.snapshot.entities.len())
            ),
        }
        // Refused whole: the decoder still holds the full.
        assert_eq!(dec.current().map(|w| w.entities.len()), Some(half as usize));
    }

    /// One of every field kind.
    fn test_schema() -> SnapshotSchema {
        SnapshotSchema::new(vec![
            FieldDef::new("alive", FieldKind::Bool),
            FieldDef::new("team", FieldKind::U8),
            FieldDef::new("flags16", FieldKind::U16),
            FieldDef::new("score", FieldKind::U32),
            FieldDef::new("delta", FieldKind::I32),
            FieldDef::new(
                "health",
                FieldKind::Float {
                    min: 0.0,
                    max: 100.0,
                    bits: 12,
                },
            ),
            FieldDef::new(
                "pos",
                FieldKind::Vec3Quantised {
                    min: Vec3::new(-500.0, -50.0, -500.0),
                    max: Vec3::new(500.0, 200.0, 500.0),
                    bits: 16,
                },
            ),
            FieldDef::new("rot", FieldKind::QuatSmallestThree { bits: 10 }),
            FieldDef::new("state", FieldKind::Enum { variants: 5 }),
        ])
        .expect("test schema is valid")
    }

    fn random_entity(rng: &mut Rng, id: u32) -> EntitySnapshot {
        EntitySnapshot::new(
            NetId(id),
            vec![
                FieldValue::Bool(rng.below(2) == 1),
                FieldValue::U8(rng.below(256) as u8),
                FieldValue::U16(rng.below(65_536) as u16),
                FieldValue::U32(rng.next_u64() as u32),
                FieldValue::I32(rng.next_u64() as u32 as i32),
                FieldValue::F32(rng.f32_range(-10.0, 110.0)),
                FieldValue::Vec3(Vec3::new(
                    rng.f32_range(-600.0, 600.0),
                    rng.f32_range(-80.0, 260.0),
                    rng.f32_range(-600.0, 600.0),
                )),
                FieldValue::Quat(rng.quat()),
                FieldValue::Enum(rng.below(5) as u32),
            ],
        )
    }

    fn fixed_entity(id: u32, score: u32) -> EntitySnapshot {
        EntitySnapshot::new(
            NetId(id),
            vec![
                FieldValue::Bool(true),
                FieldValue::U8(1),
                FieldValue::U16(2),
                FieldValue::U32(score),
                FieldValue::I32(-3),
                FieldValue::F32(50.0),
                FieldValue::Vec3(Vec3::new(1.0, 2.0, 3.0)),
                FieldValue::Quat(Quat::IDENTITY),
                FieldValue::Enum(1),
            ],
        )
    }

    fn random_world(seed: u64, count: usize) -> WorldSnapshot {
        let mut rng = Rng::new(seed);
        WorldSnapshot::new(
            (0..count)
                .map(|i| random_entity(&mut rng, 1 + 7 * i as u32))
                .collect(),
        )
    }

    /// The snapshot a decoder must reconstruct: the quantised original, id-sorted, priorities 0.
    fn expected(schema: &SnapshotSchema, world: &WorldSnapshot) -> WorldSnapshot {
        let mut quantised = schema.quantize_snapshot(world).expect("quantise");
        quantised.entities.sort_by_key(|e| e.id);
        for entity in &mut quantised.entities {
            entity.priority = 0;
        }
        quantised
    }

    fn pair() -> (SnapshotEncoder, SnapshotDecoder) {
        let schema = test_schema();
        (SnapshotEncoder::new(schema.clone()), SnapshotDecoder::new(schema))
    }

    fn as_f32(value: &FieldValue) -> f32 {
        match value {
            FieldValue::F32(v) => *v,
            other => panic!("expected f32, got {}", other.kind_name()),
        }
    }

    fn as_vec3(value: &FieldValue) -> Vec3 {
        match value {
            FieldValue::Vec3(v) => *v,
            other => panic!("expected vec3, got {}", other.kind_name()),
        }
    }

    fn as_quat(value: &FieldValue) -> Quat {
        match value {
            FieldValue::Quat(v) => *v,
            other => panic!("expected quat, got {}", other.kind_name()),
        }
    }

    #[test]
    fn crc32_matches_the_ieee_check_value() {
        assert_eq!(crc32_ieee(b""), 0);
        assert_eq!(crc32_ieee(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn round_trip_of_random_snapshots_500_seeds() {
        for seed in 0..500u64 {
            let (mut enc, mut dec) = pair();
            let world = random_world(seed, 1 + (seed % 8) as usize);
            let packet = enc
                .encode(NetTick(seed as u32), &world, None, 65_536)
                .expect("encode");
            let decoded = dec.decode(&packet).expect("decode");
            assert_eq!(decoded.seq, Seq(1), "seed {seed}");
            assert_eq!(decoded.tick, NetTick(seed as u32), "seed {seed}");
            assert!(decoded.is_full && decoded.complete, "seed {seed}");
            assert_eq!(decoded.snapshot, expected(enc.schema(), &world), "seed {seed}");
        }
    }

    #[test]
    fn exact_kinds_round_trip_at_their_boundaries() {
        let (mut enc, mut dec) = pair();
        let world = WorldSnapshot::new(vec![
            EntitySnapshot::new(
                NetId(1),
                vec![
                    FieldValue::Bool(false),
                    FieldValue::U8(0),
                    FieldValue::U16(0),
                    FieldValue::U32(0),
                    FieldValue::I32(i32::MIN),
                    FieldValue::F32(0.0),
                    FieldValue::Vec3(Vec3::new(-500.0, -50.0, -500.0)),
                    FieldValue::Quat(Quat::IDENTITY),
                    FieldValue::Enum(0),
                ],
            ),
            EntitySnapshot::new(
                NetId(2),
                vec![
                    FieldValue::Bool(true),
                    FieldValue::U8(u8::MAX),
                    FieldValue::U16(u16::MAX),
                    FieldValue::U32(u32::MAX),
                    FieldValue::I32(i32::MAX),
                    FieldValue::F32(100.0),
                    FieldValue::Vec3(Vec3::new(500.0, 200.0, 500.0)),
                    FieldValue::Quat(Quat::from_xyzw(1.0, 0.0, 0.0, 0.0)),
                    FieldValue::Enum(4),
                ],
            ),
        ]);
        let packet = enc.encode(NetTick(0), &world, None, 4096).expect("encode");
        let decoded = dec.decode(&packet).expect("decode");
        assert_eq!(decoded.snapshot, expected(enc.schema(), &world));
        let first = decoded.snapshot.fields_of(NetId(1)).expect("entity 1");
        assert_eq!(first[4], FieldValue::I32(i32::MIN));
        let second = decoded.snapshot.fields_of(NetId(2)).expect("entity 2");
        assert_eq!(second[1], FieldValue::U8(u8::MAX));
        assert_eq!(second[2], FieldValue::U16(u16::MAX));
        assert_eq!(second[3], FieldValue::U32(u32::MAX));
        assert_eq!(second[4], FieldValue::I32(i32::MAX));
    }

    #[test]
    fn empty_world_round_trips() {
        let (mut enc, mut dec) = pair();
        let packet = enc
            .encode(NetTick(3), &WorldSnapshot::empty(), None, 1024)
            .expect("encode");
        let decoded = dec.decode(&packet).expect("decode");
        assert!(decoded.snapshot.is_empty());
        assert!(decoded.added.is_empty());
        assert_eq!(dec.latest_baseline(), Some(Seq(1)));
    }

    #[test]
    fn delta_is_far_smaller_than_full_when_one_field_changes() {
        let (mut enc, mut dec) = pair();
        let world = random_world(7, 32);
        let full = enc.encode(NetTick(1), &world, None, 65_536).expect("full");
        dec.decode(&full).expect("decode full");
        let mut changed = world.clone();
        changed.entities[0].fields[SCORE] = FieldValue::U32(123_456);
        let (delta, report) = enc
            .encode_with_report(NetTick(2), &changed, dec.latest_baseline(), 65_536)
            .expect("delta");
        assert!(!report.is_full);
        assert_eq!(report.baseline, Some(Seq(1)));
        assert!(
            delta.len() * 4 < full.len(),
            "delta {} bytes, full {} bytes",
            delta.len(),
            full.len()
        );
        let decoded = dec.decode(&delta).expect("decode delta");
        assert_eq!(decoded.snapshot, expected(enc.schema(), &changed));
        assert_eq!(decoded.updated.len(), 1);
    }

    #[test]
    fn unchanged_world_produces_an_empty_delta() {
        let (mut enc, mut dec) = pair();
        let world = random_world(11, 6);
        let full = enc.encode(NetTick(1), &world, None, 65_536).expect("full");
        dec.decode(&full).expect("decode full");
        let (delta, report) = enc
            .encode_with_report(NetTick(2), &world, Some(Seq(1)), 65_536)
            .expect("delta");
        assert!(report.included.is_empty() && report.complete);
        assert!(delta.len() < full.len());
        let decoded = dec.decode(&delta).expect("decode delta");
        assert_eq!(decoded.snapshot, expected(enc.schema(), &world));
        assert!(decoded.added.is_empty());
        assert!(decoded.updated.is_empty());
        assert!(decoded.removed.is_empty());
    }

    #[test]
    fn sub_quantum_change_is_not_sent() {
        let (mut enc, mut dec) = pair();
        let world = expected(enc.schema(), &random_world(13, 3));
        let full = enc.encode(NetTick(1), &world, None, 65_536).expect("full");
        let first = dec.decode(&full).expect("decode full");
        let mut nudged = world.clone();
        let health = as_f32(&nudged.entities[0].fields[HEALTH]);
        // The health step is 100 / 4095, so 0.001 is well under half a step.
        nudged.entities[0].fields[HEALTH] = FieldValue::F32(health + 0.001);
        let (delta, report) = enc
            .encode_with_report(NetTick(2), &nudged, Some(Seq(1)), 65_536)
            .expect("delta");
        assert!(report.included.is_empty());
        let decoded = dec.decode(&delta).expect("decode delta");
        assert_eq!(decoded.snapshot, first.snapshot);
    }

    #[test]
    fn add_update_and_remove_in_one_delta() {
        let (mut enc, mut dec) = pair();
        let world = WorldSnapshot::new(vec![fixed_entity(1, 10), fixed_entity(2, 20)]);
        let full = enc.encode(NetTick(1), &world, None, 4096).expect("full");
        dec.decode(&full).expect("decode full");
        let next = WorldSnapshot::new(vec![fixed_entity(2, 21), fixed_entity(3, 30)]);
        let delta = enc.encode(NetTick(2), &next, Some(Seq(1)), 4096).expect("delta");
        let decoded = dec.decode(&delta).expect("decode delta");
        assert_eq!(decoded.added, vec![NetId(3)]);
        assert_eq!(decoded.updated, vec![NetId(2)]);
        assert_eq!(decoded.removed, vec![NetId(1)]);
        assert_eq!(decoded.snapshot, expected(enc.schema(), &next));
    }

    #[test]
    fn lost_packet_still_decodes_against_the_older_ack() {
        let (mut enc, mut dec) = pair();
        let w1 = WorldSnapshot::new(vec![fixed_entity(1, 1)]);
        let w2 = WorldSnapshot::new(vec![fixed_entity(1, 2)]);
        let w3 = WorldSnapshot::new(vec![fixed_entity(1, 3), fixed_entity(9, 9)]);
        let p1 = enc.encode(NetTick(1), &w1, None, 4096).expect("p1");
        dec.decode(&p1).expect("decode p1");
        // p2 is encoded but never delivered, so the client keeps acknowledging seq 1.
        let _lost = enc.encode(NetTick(2), &w2, Some(Seq(1)), 4096).expect("p2");
        let (p3, report) = enc
            .encode_with_report(NetTick(3), &w3, Some(Seq(1)), 4096)
            .expect("p3");
        assert_eq!(report.baseline, Some(Seq(1)));
        let decoded = dec.decode(&p3).expect("decode p3");
        assert_eq!(decoded.snapshot, expected(enc.schema(), &w3));
    }

    #[test]
    fn evicted_baseline_falls_back_to_a_full_snapshot() {
        let schema = test_schema();
        let mut enc = SnapshotEncoder::with_capacity(schema.clone(), 2);
        let mut dec = SnapshotDecoder::with_capacity(schema, 8);
        let world = |score| WorldSnapshot::new(vec![fixed_entity(1, score)]);
        let p1 = enc.encode(NetTick(1), &world(1), None, 4096).expect("p1");
        dec.decode(&p1).expect("decode p1");
        let _p2 = enc.encode(NetTick(2), &world(2), Some(Seq(1)), 4096).expect("p2");
        let _p3 = enc.encode(NetTick(3), &world(3), Some(Seq(1)), 4096).expect("p3");
        assert!(!enc.has_baseline(Seq(1)), "seq 1 fell out of the ring");
        let (p4, report) = enc
            .encode_with_report(NetTick(4), &world(4), Some(Seq(1)), 4096)
            .expect("p4");
        assert!(report.is_full);
        assert_eq!(report.baseline, None);
        let decoded = dec.decode(&p4).expect("decode full p4");
        assert_eq!(decoded.snapshot, expected(enc.schema(), &world(4)));
    }

    #[test]
    fn decoder_missing_baseline_errors_and_keeps_its_state() {
        let schema = test_schema();
        let mut enc = SnapshotEncoder::new(schema.clone());
        let mut dec = SnapshotDecoder::with_capacity(schema, 1);
        let world = |score| WorldSnapshot::new(vec![fixed_entity(1, score)]);
        let p1 = enc.encode(NetTick(1), &world(1), None, 4096).expect("p1");
        dec.decode(&p1).expect("decode p1");
        let p2 = enc.encode(NetTick(2), &world(2), Some(Seq(1)), 4096).expect("p2");
        dec.decode(&p2).expect("decode p2");
        assert!(!dec.has_baseline(Seq(1)), "decoder ring holds one entry");
        let held = expected(enc.schema(), &world(2));
        // The encoder still holds seq 1, so this is a delta the decoder cannot apply.
        let p3 = enc.encode(NetTick(3), &world(3), Some(Seq(1)), 4096).expect("p3");
        assert_eq!(
            dec.decode(&p3).expect_err("baseline is gone"),
            SnapshotError::BaselineMissing { seq: Seq(1) }
        );
        assert_eq!(dec.current(), Some(&held));
        assert_eq!(dec.last_seq(), Some(Seq(2)));
    }

    #[test]
    fn truncation_at_every_byte_offset_is_an_error() {
        let (mut enc, mut dec) = pair();
        let packet = enc
            .encode(NetTick(5), &random_world(21, 5), None, 65_536)
            .expect("encode");
        dec.decode(&packet).expect("the whole packet decodes");
        for cut in 0..packet.len() {
            assert!(dec.decode(&packet[..cut]).is_err(), "cut at {cut} must error");
        }
    }

    #[test]
    fn extension_and_every_single_byte_corruption_are_errors() {
        let (mut enc, mut dec) = pair();
        let packet = enc
            .encode(NetTick(5), &random_world(22, 4), None, 65_536)
            .expect("encode");
        let mut extended = packet.clone();
        extended.push(0);
        assert!(dec.decode(&extended).is_err(), "trailing byte");
        for at in 0..packet.len() {
            let mut damaged = packet.clone();
            if let Some(byte) = damaged.get_mut(at) {
                *byte ^= 0xff;
            }
            assert!(dec.decode(&damaged).is_err(), "byte {at} flipped");
        }
    }

    #[test]
    fn ten_thousand_random_inputs_never_panic() {
        let mut dec = SnapshotDecoder::new(test_schema());
        let mut rng = Rng::new(0x5eed);
        for _ in 0..10_000 {
            let len = rng.below(65) as usize;
            let buf: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
            let _result = dec.decode(&buf);
        }
        // The decoder is still usable afterwards.
        let (mut enc, mut fresh) = pair();
        let packet = enc
            .encode(NetTick(1), &random_world(3, 2), None, 4096)
            .expect("encode");
        fresh.decode(&packet).expect("decode after fuzz inputs");
    }

    #[test]
    fn header_guards_reject_garbage_before_the_body_is_read() {
        let (mut enc, mut dec) = pair();
        assert!(matches!(dec.decode(&[]), Err(SnapshotError::TooShort { .. })));
        assert!(dec.decode(&[PACKET_MAGIC]).is_err());
        let packet = enc
            .encode(NetTick(1), &random_world(4, 1), None, 4096)
            .expect("encode");
        let mut bad_magic = packet.clone();
        if let Some(byte) = bad_magic.get_mut(0) {
            *byte = 0;
        }
        assert!(matches!(
            dec.decode(&bad_magic),
            Err(SnapshotError::BadMagic { .. })
        ));
        let mut bad_version = packet.clone();
        if let Some(byte) = bad_version.get_mut(1) {
            *byte = PACKET_VERSION.wrapping_add(1);
        }
        assert!(matches!(
            dec.decode(&bad_version),
            Err(SnapshotError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn quantisation_error_stays_within_half_a_step() {
        let schema = test_schema();
        let float_bound = schema
            .field(HEALTH)
            .expect("health field")
            .kind
            .max_quantisation_error()
            .expect("float bound");
        let vec_bound = schema
            .field(POS)
            .expect("pos field")
            .kind
            .max_quantisation_error()
            .expect("vec3 bound");
        let mut rng = Rng::new(42);
        for _ in 0..2000 {
            let health = rng.f32_range(0.0, 100.0);
            let quantised = schema
                .quantize_value(HEALTH, &FieldValue::F32(health))
                .expect("quantise health");
            assert!(
                (as_f32(&quantised) - health).abs() <= float_bound * 1.001 + 1e-4,
                "{health} -> {quantised:?}"
            );
            let pos = Vec3::new(
                rng.f32_range(-500.0, 500.0),
                rng.f32_range(-50.0, 200.0),
                rng.f32_range(-500.0, 500.0),
            );
            let quantised = schema
                .quantize_value(POS, &FieldValue::Vec3(pos))
                .expect("quantise pos");
            let got = as_vec3(&quantised);
            for (want, have) in [pos.x, pos.y, pos.z].iter().zip([got.x, got.y, got.z].iter()) {
                assert!((have - want).abs() <= vec_bound * 1.001 + 1e-3, "{pos} -> {got}");
            }
        }
    }

    #[test]
    fn quat_smallest_three_is_unit_and_close() {
        let schema = test_schema();
        let mut rng = Rng::new(43);
        for _ in 0..2000 {
            let rot = rng.quat();
            let quantised = schema
                .quantize_value(ROT, &FieldValue::Quat(rot))
                .expect("quantise rot");
            let got = as_quat(&quantised);
            let len2 = got.x * got.x + got.y * got.y + got.z * got.z + got.w * got.w;
            assert!((len2 - 1.0).abs() < 1e-4, "length squared {len2}");
            assert!(
                got.angle_between(rot) < 0.01,
                "angle {} for {rot:?}",
                got.angle_between(rot)
            );
        }
    }

    #[test]
    fn non_finite_and_degenerate_inputs_clamp_instead_of_failing() {
        let (mut enc, mut dec) = pair();
        let mut entity = fixed_entity(1, 7);
        entity.fields[HEALTH] = FieldValue::F32(f32::NAN);
        entity.fields[POS] = FieldValue::Vec3(Vec3::new(f32::INFINITY, f32::NEG_INFINITY, f32::NAN));
        entity.fields[ROT] = FieldValue::Quat(Quat::from_xyzw(0.0, 0.0, 0.0, 0.0));
        let world = WorldSnapshot::new(vec![entity]);
        let packet = enc.encode(NetTick(1), &world, None, 4096).expect("encode");
        let decoded = dec.decode(&packet).expect("decode");
        let fields = decoded.snapshot.fields_of(NetId(1)).expect("entity");
        assert_eq!(as_f32(&fields[HEALTH]), 0.0, "NaN clamps to the range start");
        let pos = as_vec3(&fields[POS]);
        assert!((pos.x - 500.0).abs() < 1e-3, "+inf clamps to max, got {pos}");
        assert!((pos.y + 50.0).abs() < 1e-3, "-inf clamps to min, got {pos}");
        assert!((pos.z + 500.0).abs() < 1e-3, "NaN clamps to min, got {pos}");
        assert!(
            as_quat(&fields[ROT]).angle_between(Quat::IDENTITY) < 0.01,
            "a zero quaternion travels as the identity"
        );
    }

    #[test]
    fn out_of_range_floats_clamp_to_the_range_ends() {
        let schema = test_schema();
        let low = schema
            .quantize_value(HEALTH, &FieldValue::F32(-5.0))
            .expect("low");
        assert_eq!(as_f32(&low), 0.0);
        let high = schema
            .quantize_value(HEALTH, &FieldValue::F32(1.0e30))
            .expect("high");
        assert!((as_f32(&high) - 100.0).abs() < 1e-3);
    }

    #[test]
    fn encoding_is_deterministic_and_order_independent() {
        let world = random_world(31, 9);
        let mut first = SnapshotEncoder::new(test_schema());
        let mut second = SnapshotEncoder::new(test_schema());
        let a = first.encode(NetTick(8), &world, None, 65_536).expect("a");
        let b = second.encode(NetTick(8), &world, None, 65_536).expect("b");
        assert_eq!(a, b, "same inputs, same bytes");
        let mut shuffled = world.clone();
        shuffled.entities.reverse();
        let mut third = SnapshotEncoder::new(test_schema());
        let c = third.encode(NetTick(8), &shuffled, None, 65_536).expect("c");
        assert_eq!(a, c, "input order must not change the bytes");
    }

    #[test]
    fn budget_is_respected_and_omissions_are_reported() {
        let world = random_world(51, 20);
        let mut probe = SnapshotEncoder::new(test_schema());
        let full_len = probe
            .encode(NetTick(1), &world, None, 65_536)
            .expect("full")
            .len();
        assert!(full_len > 120, "precondition: full is {full_len} bytes");
        let (mut enc, mut dec) = pair();
        let (packet, report) = enc
            .encode_with_report(NetTick(1), &world, None, 120)
            .expect("budgeted encode");
        assert!(packet.len() <= 120);
        assert_eq!(report.bytes, packet.len());
        assert!(!report.complete);
        assert!(!report.included.is_empty());
        assert!(!report.omitted.is_empty());
        let mut accounted = report.included.clone();
        accounted.extend(report.omitted.iter().copied());
        accounted.sort_unstable();
        let mut all: Vec<NetId> = world.ids().collect();
        all.sort_unstable();
        assert_eq!(accounted, all, "every entity is included or omitted");
        let decoded = dec.decode(&packet).expect("decode budgeted packet");
        assert_eq!(decoded.snapshot.ids().collect::<Vec<_>>(), report.included);
        assert!(!decoded.complete);
        // The incomplete packet is a baseline on both sides, holding what it carried.
        assert_eq!(dec.latest_baseline(), Some(report.seq));
        assert!(enc.has_baseline(report.seq));
    }

    #[test]
    fn budget_packing_prefers_higher_priority_entities() {
        let world = WorldSnapshot::new(vec![
            fixed_entity(1, 1),
            fixed_entity(2, 2),
            fixed_entity(3, 3),
            fixed_entity(4, 4),
            fixed_entity(5, 5).with_priority(50),
            fixed_entity(6, 6).with_priority(100),
        ]);
        let mut probe = SnapshotEncoder::new(test_schema());
        let full_len = probe
            .encode(NetTick(1), &world, None, 65_536)
            .expect("full")
            .len();
        let mut enc = SnapshotEncoder::new(test_schema());
        let (packet, report) = enc
            .encode_with_report(NetTick(1), &world, None, full_len / 2)
            .expect("budgeted encode");
        assert!(packet.len() <= full_len / 2);
        assert!(report.included.contains(&NetId(6)), "{report:?}");
        assert!(report.included.contains(&NetId(5)), "{report:?}");
        assert!(!report.omitted.is_empty(), "{report:?}");
    }

    #[test]
    fn too_small_a_budget_errors_without_consuming_a_sequence() {
        let (mut enc, _dec) = pair();
        let world = random_world(52, 2);
        assert!(matches!(
            enc.encode(NetTick(1), &world, None, 0),
            Err(SnapshotError::BudgetTooSmall { .. })
        ));
        assert!(matches!(
            enc.encode(NetTick(1), &world, None, 5),
            Err(SnapshotError::BudgetTooSmall { .. })
        ));
        assert_eq!(enc.next_seq(), Seq(1));
        assert!(enc.last_report().is_none());
        let packet = enc
            .encode(NetTick(1), &world, None, 4096)
            .expect("encode after failures");
        assert!(!packet.is_empty());
        assert_eq!(enc.next_seq(), Seq(2));
    }

    #[test]
    fn ack_for_an_incomplete_packet_deltas_only_the_omitted_entities() {
        let world = random_world(53, 12);
        let (mut enc, mut dec) = pair();
        let (packet, report) = enc
            .encode_with_report(NetTick(1), &world, None, 96)
            .expect("budgeted encode");
        assert!(!report.complete);
        dec.decode(&packet).expect("decode incomplete");
        let (next, next_report) = enc
            .encode_with_report(NetTick(2), &world, Some(report.seq), 65_536)
            .expect("next encode");
        assert!(!next_report.is_full, "an incomplete packet is a baseline");
        assert_eq!(next_report.baseline, Some(report.seq));
        assert_eq!(
            next_report.included, report.omitted,
            "only what was left out is resent"
        );
        let decoded = dec.decode(&next).expect("decode delta");
        assert_eq!(decoded.added, report.omitted);
        assert_eq!(decoded.snapshot, expected(enc.schema(), &world));
    }

    #[test]
    fn invalid_schemas_are_rejected() {
        assert_eq!(
            SnapshotSchema::new(Vec::new()).expect_err("empty"),
            SnapshotError::EmptySchema
        );
        assert!(matches!(
            SnapshotSchema::new(vec![
                FieldDef::new("a", FieldKind::Bool),
                FieldDef::new("a", FieldKind::U8),
            ]),
            Err(SnapshotError::DuplicateField(_))
        ));
        let invalid_kinds = [
            FieldKind::Float {
                min: 1.0,
                max: 1.0,
                bits: 8,
            },
            FieldKind::Float {
                min: f32::NAN,
                max: 1.0,
                bits: 8,
            },
            FieldKind::Float {
                min: 0.0,
                max: 1.0,
                bits: 0,
            },
            FieldKind::Float {
                min: 0.0,
                max: 1.0,
                bits: 33,
            },
            FieldKind::Vec3Quantised {
                min: Vec3::new(0.0, 0.0, 0.0),
                max: Vec3::new(1.0, 0.0, 1.0),
                bits: 8,
            },
            FieldKind::Vec3Quantised {
                min: Vec3::new(0.0, 0.0, 0.0),
                max: Vec3::new(1.0, 1.0, 1.0),
                bits: 0,
            },
            FieldKind::QuatSmallestThree { bits: 1 },
            FieldKind::QuatSmallestThree { bits: 33 },
            FieldKind::Enum { variants: 1 },
            FieldKind::Enum { variants: 0 },
        ];
        for kind in invalid_kinds {
            assert!(
                matches!(
                    SnapshotSchema::new(vec![FieldDef::new("f", kind)]),
                    Err(SnapshotError::InvalidField { .. })
                ),
                "{kind:?} must be rejected"
            );
        }
        assert!(SnapshotSchema::new(vec![FieldDef::new("", FieldKind::Bool)]).is_err());
        let too_many: Vec<FieldDef> = (0..=MAX_FIELDS)
            .map(|i| FieldDef::new(format!("f{i}"), FieldKind::Bool))
            .collect();
        assert!(matches!(
            SnapshotSchema::new(too_many),
            Err(SnapshotError::TooManyFields { .. })
        ));
    }

    #[test]
    fn layout_hash_distinguishes_schemas_and_the_decoder_enforces_it() {
        let schema = test_schema();
        assert_eq!(schema.layout_hash(), test_schema().layout_hash());
        let mut fields = schema.fields().to_vec();
        if let Some(field) = fields.get_mut(HEALTH) {
            *field = FieldDef::new(
                "health",
                FieldKind::Float {
                    min: 0.0,
                    max: 100.0,
                    bits: 13,
                },
            );
        }
        let other = SnapshotSchema::new(fields).expect("other schema");
        assert_ne!(schema.layout_hash(), other.layout_hash());
        let mut enc = SnapshotEncoder::new(schema);
        let packet = enc
            .encode(NetTick(1), &random_world(61, 1), None, 4096)
            .expect("encode");
        let mut dec = SnapshotDecoder::new(other);
        assert!(matches!(
            dec.decode(&packet),
            Err(SnapshotError::SchemaMismatch { .. })
        ));
    }

    #[test]
    fn malformed_worlds_are_rejected_on_encode() {
        let (mut enc, _dec) = pair();
        let duplicate = WorldSnapshot::new(vec![fixed_entity(1, 1), fixed_entity(1, 2)]);
        assert!(matches!(
            enc.encode(NetTick(1), &duplicate, None, 4096),
            Err(SnapshotError::DuplicateEntity(NetId(1)))
        ));
        let mut short = fixed_entity(1, 1);
        short.fields.pop();
        assert!(matches!(
            enc.encode(NetTick(1), &WorldSnapshot::new(vec![short]), None, 4096),
            Err(SnapshotError::FieldCountMismatch { .. })
        ));
        let mut mistyped = fixed_entity(1, 1);
        mistyped.fields[0] = FieldValue::U8(3);
        assert!(matches!(
            enc.encode(NetTick(1), &WorldSnapshot::new(vec![mistyped]), None, 4096),
            Err(SnapshotError::FieldTypeMismatch { .. })
        ));
        let mut bad_enum = fixed_entity(1, 1);
        bad_enum.fields[8] = FieldValue::Enum(5);
        assert!(matches!(
            enc.encode(NetTick(1), &WorldSnapshot::new(vec![bad_enum]), None, 4096),
            Err(SnapshotError::EnumOutOfRange { .. })
        ));
        assert_eq!(enc.next_seq(), Seq(1), "failures consume no sequence");
    }

    #[test]
    fn quantize_value_reports_a_bad_field_index() {
        let schema = test_schema();
        assert!(matches!(
            schema.quantize_value(99, &FieldValue::Bool(true)),
            Err(SnapshotError::InvalidFieldIndex { .. })
        ));
        assert!(matches!(
            schema.quantize_value(0, &FieldValue::U32(1)),
            Err(SnapshotError::FieldTypeMismatch { .. })
        ));
    }

    #[test]
    fn sixty_tick_delta_chain_with_a_loss_stays_exact() {
        let (mut enc, mut dec) = pair();
        let mut world = random_world(71, 5);
        let dropped_id = world.entities[0].id;
        for tick in 1..=60u32 {
            for entity in &mut world.entities {
                if let FieldValue::U32(score) = &mut entity.fields[SCORE] {
                    *score = score.wrapping_add(tick);
                }
                if let FieldValue::F32(health) = &mut entity.fields[HEALTH] {
                    *health += 1.5;
                }
            }
            if tick == 10 {
                world.entities.retain(|e| e.id != dropped_id);
            }
            if tick == 20 {
                world.entities.push(fixed_entity(999, 20));
            }
            let packet = enc
                .encode(NetTick(tick), &world, dec.latest_baseline(), 65_536)
                .expect("encode");
            if tick == 30 {
                continue; // lost in transit: the ack stays at tick 29's sequence
            }
            let decoded = dec.decode(&packet).expect("decode");
            assert_eq!(decoded.tick, NetTick(tick), "tick {tick}");
            assert_eq!(decoded.snapshot, expected(enc.schema(), &world), "tick {tick}");
        }
    }

    #[test]
    fn every_kind_round_trips_alone_at_extreme_widths() {
        let cases: Vec<(FieldKind, FieldValue)> = vec![
            (FieldKind::Bool, FieldValue::Bool(true)),
            (FieldKind::U8, FieldValue::U8(200)),
            (FieldKind::U16, FieldValue::U16(60_000)),
            (FieldKind::U32, FieldValue::U32(u32::MAX)),
            (FieldKind::I32, FieldValue::I32(i32::MIN)),
            (
                FieldKind::Float {
                    min: 0.0,
                    max: 1.0,
                    bits: 1,
                },
                FieldValue::F32(0.25),
            ),
            (
                FieldKind::Float {
                    min: -1.0,
                    max: 1.0,
                    bits: 32,
                },
                FieldValue::F32(0.123_456_79),
            ),
            (
                FieldKind::Vec3Quantised {
                    min: Vec3::new(-1.0, -2.0, -3.0),
                    max: Vec3::new(1.0, 2.0, 3.0),
                    bits: 1,
                },
                FieldValue::Vec3(Vec3::new(0.9, -1.9, 0.1)),
            ),
            (
                FieldKind::QuatSmallestThree { bits: 2 },
                FieldValue::Quat(Quat::from_rotation_y(1.0)),
            ),
            (
                FieldKind::QuatSmallestThree { bits: 32 },
                FieldValue::Quat(Quat::from_rotation_y(1.0)),
            ),
            (FieldKind::Enum { variants: 2 }, FieldValue::Enum(1)),
            (FieldKind::Enum { variants: 3 }, FieldValue::Enum(2)),
            (FieldKind::Enum { variants: 256 }, FieldValue::Enum(255)),
        ];
        for (kind, value) in cases {
            let schema = SnapshotSchema::new(vec![FieldDef::new("only", kind)]).expect("schema");
            let world = WorldSnapshot::new(vec![EntitySnapshot::new(NetId(4), vec![value])]);
            let mut enc = SnapshotEncoder::new(schema.clone());
            let mut dec = SnapshotDecoder::new(schema.clone());
            let packet = enc.encode(NetTick(0), &world, None, 4096).expect("encode");
            let decoded = dec.decode(&packet).expect("decode");
            assert_eq!(decoded.snapshot, expected(&schema, &world), "{kind:?}");
        }
    }

    #[test]
    fn a_max_width_schema_round_trips_and_deltas() {
        let fields: Vec<FieldDef> = (0..MAX_FIELDS)
            .map(|i| FieldDef::new(format!("f{i}"), FieldKind::Bool))
            .collect();
        let schema = SnapshotSchema::new(fields).expect("128 fields");
        let values = |last: bool| -> Vec<FieldValue> {
            (0..MAX_FIELDS)
                .map(|i| FieldValue::Bool(if i + 1 == MAX_FIELDS { last } else { i % 2 == 0 }))
                .collect()
        };
        let w1 = WorldSnapshot::new(vec![EntitySnapshot::new(NetId(1), values(false))]);
        let w2 = WorldSnapshot::new(vec![EntitySnapshot::new(NetId(1), values(true))]);
        let mut enc = SnapshotEncoder::new(schema.clone());
        let mut dec = SnapshotDecoder::new(schema);
        let p1 = enc.encode(NetTick(1), &w1, None, 4096).expect("p1");
        dec.decode(&p1).expect("decode p1");
        let p2 = enc.encode(NetTick(2), &w2, Some(Seq(1)), 4096).expect("p2");
        let decoded = dec.decode(&p2).expect("decode p2");
        assert_eq!(decoded.updated, vec![NetId(1)]);
        assert_eq!(
            decoded.snapshot,
            expected(enc.schema(), &w2),
            "the 128th field's change survived the mask"
        );
    }

    #[test]
    fn extreme_ids_and_ticks_round_trip() {
        let (mut enc, mut dec) = pair();
        let world = WorldSnapshot::new(vec![
            fixed_entity(0, 1),
            fixed_entity(u32::MAX, 2),
            fixed_entity(1 << 21, 3),
        ]);
        let packet = enc.encode(NetTick(u32::MAX), &world, None, 4096).expect("encode");
        let decoded = dec.decode(&packet).expect("decode");
        assert_eq!(decoded.tick, NetTick(u32::MAX));
        assert_eq!(decoded.snapshot, expected(enc.schema(), &world));
    }

    #[test]
    fn sequence_numbers_and_reports_advance() {
        let (mut enc, mut dec) = pair();
        let w1 = WorldSnapshot::new(vec![fixed_entity(1, 1)]);
        let w2 = WorldSnapshot::new(vec![fixed_entity(1, 2)]);
        assert_eq!(enc.next_seq(), Seq(1));
        let (p1, r1) = enc.encode_with_report(NetTick(42), &w1, None, 4096).expect("p1");
        assert_eq!(r1.seq, Seq(1));
        assert_eq!(r1.tick, NetTick(42));
        assert!(r1.is_full && r1.complete && r1.omitted.is_empty());
        assert_eq!(enc.last_report(), Some(&r1));
        let d1 = dec.decode(&p1).expect("decode p1");
        assert_eq!((d1.seq, d1.tick, d1.baseline), (Seq(1), NetTick(42), None));
        let (p2, r2) = enc
            .encode_with_report(NetTick(43), &w2, Some(d1.seq), 4096)
            .expect("p2");
        assert_eq!(r2.seq, Seq(2));
        assert_eq!(r2.baseline, Some(Seq(1)));
        assert_eq!(r2.included, vec![NetId(1)]);
        let d2 = dec.decode(&p2).expect("decode p2");
        assert_eq!(d2.seq, Seq(2));
        assert_eq!(d2.baseline, Some(Seq(1)));
        assert_eq!(dec.latest_baseline(), Some(Seq(2)));
        assert_eq!(enc.baseline_count(), 2);
        assert_eq!(dec.baseline_count(), 2);
    }

    // ---- Verifier tests (lane sprint-muse-verify-snapdelta) ------------------------------------

    /// Wraps `body` in a valid outer header (magic, version, length, CRC), so a test reaches the
    /// bit-packed body parser instead of stopping at the header guards.
    fn framed(body: &[u8]) -> Vec<u8> {
        let mut packet = Vec::with_capacity(OUTER_HEADER_BYTES + body.len());
        packet.push(PACKET_MAGIC);
        packet.push(PACKET_VERSION);
        let len = u32::try_from(OUTER_HEADER_BYTES + body.len()).expect("test body fits u32");
        packet.extend_from_slice(&len.to_le_bytes());
        packet.extend_from_slice(&crc32_ieee(body).to_le_bytes());
        packet.extend_from_slice(body);
        packet
    }

    #[test]
    fn a_sustained_budget_still_converges_to_the_whole_world() {
        // A static world far larger than one packet: a budgeted codec must make progress every
        // packet (what was sent is what the client holds), so the client converges.
        let world = WorldSnapshot::new((1..=40).map(|i| fixed_entity(i, i * 3)).collect());
        let (mut enc, mut dec) = pair();
        let budget = 200;
        let mut converged_at = None;
        for tick in 1..=40u32 {
            let (packet, report) = enc
                .encode_with_report(NetTick(tick), &world, dec.latest_baseline(), budget)
                .expect("encode");
            assert!(packet.len() <= budget, "tick {tick}: {} bytes", packet.len());
            let decoded = dec.decode(&packet).expect("decode");
            if decoded.snapshot == expected(enc.schema(), &world) {
                assert!(report.complete, "tick {tick}: converged on a complete packet");
                converged_at = Some(tick);
                break;
            }
        }
        // 40 records of about 20 bytes each through about 180 usable bytes: about 5 packets.
        let tick = converged_at.expect("the client never received the whole world");
        assert!(tick <= 8, "converged only at packet {tick}");
    }

    #[test]
    fn a_changing_world_under_a_tight_budget_stays_consistent_and_converges() {
        // Every packet is budgeted below the full diff, one in five is lost, and priorities shuffle
        // every tick. Every packet must still decode against its baseline, and once the world
        // stops changing (tick 60) the client converges within 40 ticks.
        for seed in 0..20u64 {
            let mut rng = Rng::new(seed);
            let mut world = random_world(seed, 24);
            let (mut enc, mut dec) = pair();
            let mut ack = None;
            for tick in 1..=120u32 {
                if tick <= 60 {
                    for entity in &mut world.entities {
                        if rng.below(3) == 0 {
                            entity.fields[SCORE] = FieldValue::U32(rng.next_u64() as u32);
                        }
                        entity.priority = rng.below(4) as u32;
                    }
                }
                let packet = enc.encode(NetTick(tick), &world, ack, 160).expect("encode");
                // One packet in five is lost.
                if rng.below(5) == 0 {
                    continue;
                }
                let decoded = dec.decode(&packet).expect("decode");
                ack = dec.latest_baseline();
                if tick > 100 {
                    assert_eq!(
                        decoded.snapshot,
                        expected(enc.schema(), &world),
                        "seed {seed} tick {tick}: no convergence after the world stopped"
                    );
                }
            }
        }
    }

    #[test]
    fn reported_quantisation_bound_holds_at_every_width() {
        let ranges = [(-1.0f32, 1.0f32), (0.0, 100.0), (1000.0, 1001.0), (-1.0e4, 1.0e4)];
        let mut rng = Rng::new(7);
        for (min, max) in ranges {
            for bits in [1u32, 8, 16, 20, 24, 28, 32] {
                let kind = FieldKind::Float { min, max, bits };
                let schema = SnapshotSchema::new(vec![FieldDef::new("f", kind)]).expect("schema");
                let bound = kind.max_quantisation_error().expect("float bound");
                for _ in 0..500 {
                    let v = rng.f32_range(min, max).clamp(min, max);
                    let got = as_f32(&schema.quantize_value(0, &FieldValue::F32(v)).expect("q"));
                    assert!(
                        (got - v).abs() <= bound,
                        "[{min}, {max}] at {bits} bits: {v} -> {got}, error {} above bound {bound}",
                        (got - v).abs()
                    );
                }
            }
        }
    }

    #[test]
    fn framed_random_bodies_never_panic_and_never_corrupt_state() {
        // Random bodies behind a valid header, about half of them starting with the right schema
        // hash, so the record parser itself sees hostile input.
        let schema = test_schema();
        let hash = schema.layout_hash().to_le_bytes();
        let mut rng = Rng::new(0xbad5eed);
        let (mut enc, mut dec) = pair();
        let world = random_world(5, 4);
        let first = enc.encode(NetTick(1), &world, None, 65_536).expect("encode");
        dec.decode(&first).expect("decode");
        let held = dec.current().cloned();
        let mut accepted = 0;
        for i in 0..10_000 {
            let len = rng.below(48) as usize;
            let mut body: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
            if i % 2 == 0 && body.len() >= 4 {
                body[..4].copy_from_slice(&hash);
            }
            match dec.decode(&framed(&body)) {
                Ok(decoded) => {
                    accepted += 1;
                    assert_eq!(dec.current(), Some(&decoded.snapshot));
                }
                Err(_) => {
                    if accepted == 0 {
                        assert_eq!(dec.current().cloned(), held, "an error changed the state");
                    }
                }
            }
        }
        // The decoder still decodes a real packet afterwards.
        let mut fresh_enc = SnapshotEncoder::new(test_schema());
        let packet = fresh_enc
            .encode(NetTick(2), &world, None, 65_536)
            .expect("encode");
        let decoded = dec.decode(&packet).expect("decode after fuzzing");
        assert_eq!(decoded.snapshot, expected(&schema, &world));
    }

    #[test]
    fn body_bit_flips_behind_a_fixed_crc_never_panic() {
        // An attacker can always recompute the CRC: every single-bit flip of a real delta body,
        // re-framed, must decode or error, never panic, and a successful decode must still name
        // only entities with the schema's field count.
        let (mut enc, mut dec) = pair();
        let world = random_world(9, 6);
        let full = enc.encode(NetTick(1), &world, None, 65_536).expect("full");
        dec.decode(&full).expect("decode full");
        let mut next = world.clone();
        next.entities[1].fields[SCORE] = FieldValue::U32(7);
        next.entities.pop();
        next.entities.push(fixed_entity(5000, 1));
        let delta = enc
            .encode(NetTick(2), &next, Some(Seq(1)), 65_536)
            .expect("delta");
        let body = &delta[OUTER_HEADER_BYTES..];
        for bit in 0..body.len() * 8 {
            let mut damaged = body.to_vec();
            damaged[bit / 8] ^= 1 << (bit % 8);
            let mut probe = SnapshotDecoder::new(test_schema());
            probe.decode(&full).expect("decode full");
            if let Ok(decoded) = probe.decode(&framed(&damaged)) {
                for entity in &decoded.snapshot.entities {
                    assert_eq!(entity.fields.len(), test_schema().field_count());
                }
            }
        }
    }

    #[test]
    fn random_loss_over_long_runs_stays_exact() {
        for seed in 0..10u64 {
            let mut rng = Rng::new(1000 + seed);
            let mut world = random_world(seed, 10);
            let mut next_id = 10_000u32;
            let (mut enc, mut dec) = pair();
            let mut ack = None;
            for tick in 1..=300u32 {
                for entity in &mut world.entities {
                    if rng.below(4) == 0 {
                        let fresh = random_entity(&mut rng, entity.id.0);
                        let field = rng.below(9) as usize;
                        entity.fields[field] = fresh.fields[field];
                    }
                }
                if rng.below(10) == 0 && !world.entities.is_empty() {
                    let at = rng.below(world.entities.len() as u64) as usize;
                    world.entities.remove(at);
                }
                if rng.below(10) == 0 {
                    world.entities.push(random_entity(&mut rng, next_id));
                    next_id += 1;
                }
                let packet = enc.encode(NetTick(tick), &world, ack, 65_536).expect("encode");
                if rng.below(4) == 0 {
                    continue; // lost
                }
                let decoded = dec.decode(&packet).expect("decode");
                ack = dec.latest_baseline();
                assert_eq!(
                    decoded.snapshot,
                    expected(enc.schema(), &world),
                    "seed {seed} tick {tick}"
                );
            }
        }
    }

    #[test]
    fn delta_bytes_are_deterministic_across_encoders() {
        let w1 = random_world(81, 12);
        let mut w2 = w1.clone();
        w2.entities[3].fields[POS] = FieldValue::Vec3(Vec3::new(10.0, 20.0, 30.0));
        w2.entities.swap(0, 5);
        let run = || {
            let mut enc = SnapshotEncoder::new(test_schema());
            let a = enc.encode(NetTick(1), &w1, None, 65_536).expect("full");
            let b = enc.encode(NetTick(2), &w2, Some(Seq(1)), 65_536).expect("delta");
            (a, b)
        };
        assert_eq!(run(), run());
    }

    /// Spec 120 § 8.1 (R1): seq 10 and a late seq 9 both name baseline 8. Decoding 10 then 9 must
    /// leave `current()` at tick 10; the late packet still becomes a baseline a later delta may name.
    #[test]
    fn reordered_packet_never_moves_current_back() {
        let (mut enc, mut dec) = pair();
        let world = |score: u32| WorldSnapshot::new(vec![fixed_entity(1, score), fixed_entity(2, score)]);
        for tick in 1..=8u32 {
            let packet = enc
                .encode(NetTick(tick), &world(tick), dec.latest_baseline(), 4096)
                .expect("in-order packet");
            dec.decode(&packet).expect("in-order decode");
        }
        assert_eq!(dec.latest_baseline(), Some(Seq(8)));
        let p9 = enc.encode(NetTick(9), &world(9), Some(Seq(8)), 4096).expect("p9");
        let p10 = enc
            .encode(NetTick(10), &world(10), Some(Seq(8)), 4096)
            .expect("p10");
        let newest = expected(enc.schema(), &world(10));
        assert_eq!(dec.decode(&p10).expect("p10 decodes").snapshot, newest);
        let late = dec.decode(&p9).expect("a late packet still decodes");
        assert_eq!(
            dec.current(),
            Some(&newest),
            "a late seq 9 moved current() back from tick 10"
        );
        assert_eq!(
            late.snapshot, newest,
            "the answer to a late packet is current(), unchanged"
        );
        assert!(late.added.is_empty() && late.updated.is_empty() && late.removed.is_empty());
        assert_eq!(dec.last_seq(), Some(Seq(10)));
        assert_eq!(
            dec.latest_baseline(),
            Some(Seq(10)),
            "acknowledge the newest, not the latest to arrive"
        );
        assert!(dec.has_baseline(Seq(9)), "the late packet is kept as a baseline");
        // The encoder may still delta against 9 (its ack could arrive late too).
        let p11 = enc
            .encode(NetTick(11), &world(11), Some(Seq(9)), 4096)
            .expect("p11");
        assert_eq!(
            dec.decode(&p11).expect("delta on the late baseline").snapshot,
            expected(enc.schema(), &world(11))
        );
    }

    /// The bad-wifi profile of the net-soak harness (spec 120 § 15.1): 8 ms + 0-30 ms jitter,
    /// 1 % loss, 1 % reorder up to 30 ms, 0.5 % duplicates, both directions.
    fn bad_wifi(seed: u64) -> crate::conditioner::ConditionerConfig {
        crate::conditioner::ConditionerConfig {
            enabled: true,
            seed,
            loss: 0.01,
            duplicate: 0.005,
            reorder: 0.01,
            reorder_max_ms: 30,
            delay_ms: 8,
            jitter_ms: 30,
            ..Default::default()
        }
    }

    /// Runs one 60 Hz snapshot stream through a conditioner pair in virtual time. Every entity's
    /// score is the tick it was encoded at, so the client's view going backward shows as a lower
    /// score. Returns (packets decoded out of order, the decoder, the final world).
    fn reorder_stream(
        config: crate::conditioner::ConditionerConfig,
        ticks: u32,
    ) -> (usize, SnapshotDecoder, WorldSnapshot, SnapshotSchema) {
        use crate::conditioner::NetConditioner;
        let addr: std::net::SocketAddr = "127.0.0.1:9".parse().expect("addr");
        let mut down = NetConditioner::new(config.clone());
        let mut up = NetConditioner::new(crate::conditioner::ConditionerConfig {
            seed: config.seed ^ 0x55,
            ..config
        });
        let (mut enc, mut dec) = pair();
        let mut rng = Rng::new(config.seed);
        let mut world = random_world(config.seed, 16);
        let mut next_id = 50_000u32;
        let mut ack: Option<Seq> = None;
        let mut newest_seen = 0u32;
        let mut out_of_order = 0usize;
        let mut shown = 0u32;
        let score = |w: &WorldSnapshot| {
            w.entities
                .iter()
                .map(|e| match e.fields[SCORE] {
                    FieldValue::U32(v) => v,
                    _ => 0,
                })
                .max()
                .unwrap_or(0)
        };
        for tick in 1..=ticks + 30 {
            let now = u64::from(tick) * 16;
            if tick <= ticks {
                for entity in &mut world.entities {
                    entity.fields[SCORE] = FieldValue::U32(tick);
                    if rng.below(3) == 0 {
                        let fresh = random_entity(&mut rng, entity.id.0);
                        entity.fields[POS] = fresh.fields[POS];
                    }
                }
                if rng.below(20) == 0 && world.entities.len() > 4 {
                    let at = rng.below(world.entities.len() as u64) as usize;
                    world.entities.remove(at);
                }
                if rng.below(20) == 0 {
                    let mut fresh = random_entity(&mut rng, next_id);
                    fresh.fields[SCORE] = FieldValue::U32(tick);
                    world.entities.push(fresh);
                    next_id += 1;
                }
                let packet = enc.encode(NetTick(tick), &world, ack, 65_536).expect("encode");
                let mut sent = down.process(packet, addr, now);
                sent.extend(down.release_due(now));
                for (bytes, _) in sent {
                    let decoded = dec.decode(&bytes).expect("every delivered packet decodes");
                    if decoded.seq.0 < newest_seen {
                        out_of_order += 1;
                    }
                    newest_seen = newest_seen.max(decoded.seq.0);
                    let now_shown = score(dec.current().expect("decoded"));
                    assert!(
                        now_shown >= shown,
                        "seed {}: current() went back from tick {shown} to tick {now_shown} (seq {:?})",
                        config.seed,
                        decoded.seq
                    );
                    shown = now_shown;
                    if let Some(latest) = dec.latest_baseline() {
                        let mut ack_out = up.process(latest.0.to_le_bytes().to_vec(), addr, now);
                        ack_out.extend(up.release_due(now));
                        for (bytes, _) in ack_out {
                            let got = Seq(u32::from_le_bytes(bytes[..4].try_into().expect("4 bytes")));
                            // The server keeps the newest ack it has seen.
                            if ack.is_none_or(|held| got.0 > held.0) {
                                ack = Some(got);
                            }
                        }
                    }
                }
            } else {
                // Let the link drain, then one clean packet against the newest ack.
                for (bytes, _) in down.release_due(now) {
                    dec.decode(&bytes).expect("drained packet decodes");
                }
            }
        }
        let schema = enc.schema().clone();
        (out_of_order, dec, world, schema)
    }

    /// Spec 120 § 8.1 (R1) on the bad-wifi profile and the heavier wan_stress profile (15 % reorder):
    /// over 20 seeds the client's view never moves backward, and every delivered packet decodes.
    #[test]
    fn bad_wifi_reorder_never_moves_current_back() {
        let mut reordered = 0;
        for seed in 0..10u64 {
            let (out_of_order, ..) = reorder_stream(bad_wifi(seed + 1), 600);
            reordered += out_of_order;
            let stress = crate::conditioner::ConditionerConfig {
                seed: seed + 101,
                ..crate::conditioner::ConditionerConfig::from_profile("wan_stress").expect("profile")
            };
            let (out_of_order, ..) = reorder_stream(stress, 600);
            reordered += out_of_order;
        }
        assert!(
            reordered > 100,
            "precondition: the profiles reordered only {reordered} packets"
        );
    }

    /// Spec 120 § 8.1 (R1): 500 entities under a 1 KiB budget; once converged, the client's acks are
    /// lost for 70 packets, so the encoder's ring loses the acknowledged baseline and falls back to
    /// a full packet that holds only a few dozen entities. No entity may disappear client-side, and
    /// once acks resume the client converges to the exact world again.
    #[test]
    fn ring_overflow_under_budget_loses_no_entity() {
        let (mut enc, mut dec) = pair();
        let mut world = WorldSnapshot::new((1..=500).map(|i| fixed_entity(i, i)).collect());
        let all: Vec<NetId> = world.ids().collect();
        let budget = 1024;
        let mut tick = 0u32;
        let step = |world: &mut WorldSnapshot, tick: u32| {
            // Ten entities move every tick.
            for i in 0..10usize {
                let at = (tick as usize * 37 + i * 50) % world.entities.len();
                world.entities[at].fields[SCORE] = FieldValue::U32(tick * 1000 + i as u32);
            }
        };
        // Phase 1: acks flow, the client converges.
        let mut converged = false;
        while tick < 200 {
            tick += 1;
            step(&mut world, tick);
            let (packet, report) = enc
                .encode_with_report(NetTick(tick), &world, dec.latest_baseline(), budget)
                .expect("encode");
            assert!(packet.len() <= budget);
            let decoded = dec.decode(&packet).expect("decode");
            if report.complete && decoded.snapshot == expected(enc.schema(), &world) {
                converged = true;
                break;
            }
        }
        assert!(converged, "precondition: the client converged before the outage");
        let frozen = dec.latest_baseline();
        // Phase 2: 70 packets arrive, no ack reaches the server.
        let mut fulls = 0;
        for _ in 0..70 {
            tick += 1;
            step(&mut world, tick);
            let (packet, report) = enc
                .encode_with_report(NetTick(tick), &world, frozen, budget)
                .expect("encode");
            fulls += usize::from(report.is_full);
            dec.decode(&packet).expect("decode during the ack outage");
            let held: Vec<NetId> = dec.current().expect("current").ids().collect();
            assert_eq!(
                held.len(),
                all.len(),
                "tick {tick}: the client holds {} of {} entities after a budgeted full packet",
                held.len(),
                all.len()
            );
        }
        assert!(fulls > 0, "precondition: the ring overflowed into full packets");
        // Phase 3: acks resume; no entity disappears and the client converges again.
        let mut reconverged = false;
        for _ in 0..200 {
            tick += 1;
            step(&mut world, tick);
            let (packet, report) = enc
                .encode_with_report(NetTick(tick), &world, dec.latest_baseline(), budget)
                .expect("encode");
            let decoded = dec.decode(&packet).expect("decode after the outage");
            assert_eq!(
                dec.current().expect("current").len(),
                all.len(),
                "tick {tick}: an entity disappeared while re-basing"
            );
            if report.complete && decoded.snapshot == expected(enc.schema(), &world) {
                reconverged = true;
                break;
            }
        }
        assert!(reconverged, "the client never re-converged after the outage");
    }

    /// The copy-on-write map agrees with a `BTreeMap` under random inserts, replaces and removes
    /// (splits and emptied leaves included), and a clone taken before the edits never changes.
    #[test]
    fn cow_map_matches_a_btreemap_and_clones_stay_unchanged() {
        let mut rng = Rng::new(0xc0de);
        let mut map: CowMap<[u32]> =
            CowMap::from_sorted((0..100u32).map(|i| (NetId(i * 2), Arc::from(&[i][..]))));
        let mut model: std::collections::BTreeMap<NetId, u32> =
            (0..100u32).map(|i| (NetId(i * 2), i)).collect();
        let snapshot = map.clone();
        let snapshot_model = model.clone();
        for step in 0..5000u32 {
            let id = NetId(rng.below(400) as u32);
            if rng.below(3) == 0 {
                assert_eq!(
                    map.remove(id),
                    model.remove(&id).is_some(),
                    "step {step}: remove {id:?}"
                );
            } else {
                map.insert(id, Arc::from(&[step][..]));
                model.insert(id, step);
            }
            assert_eq!(map.len(), model.len(), "step {step}");
            assert_eq!(
                map.get(id).map(|v| v[0]),
                model.get(&id).copied(),
                "step {step}: get {id:?}"
            );
        }
        let got: Vec<(NetId, u32)> = map.iter().map(|(id, v)| (id, v[0])).collect();
        let want: Vec<(NetId, u32)> = model.into_iter().collect();
        assert_eq!(got, want);
        assert!(
            map.leaves
                .iter()
                .all(|leaf| !leaf.is_empty() && leaf.len() < 2 * COW_LEAF)
        );
        let kept: Vec<(NetId, u32)> = snapshot.iter().map(|(id, v)| (id, v[0])).collect();
        assert_eq!(kept, snapshot_model.into_iter().collect::<Vec<_>>());
    }

    /// Sequences compare in serial-number arithmetic, so order survives the wrap from `u32::MAX`
    /// to 1, and a late packet across the wrap still never moves `current()` back.
    #[test]
    fn serial_order_survives_the_sequence_wrap() {
        assert!(seq_newer(Seq(1), Seq(u32::MAX)));
        assert!(!seq_newer(Seq(u32::MAX), Seq(1)));
        assert!(seq_newer(Seq(11), Seq(10)) && !seq_newer(Seq(10), Seq(10)));
        let (mut enc, mut dec) = pair();
        enc.next_seq = u32::MAX - 1;
        let world = |score: u32| WorldSnapshot::new(vec![fixed_entity(1, score)]);
        let p1 = enc.encode(NetTick(1), &world(1), None, 4096).expect("p1");
        let s1 = dec.decode(&p1).expect("d1").seq;
        let p2 = enc.encode(NetTick(2), &world(2), Some(s1), 4096).expect("p2");
        let p3 = enc.encode(NetTick(3), &world(3), Some(s1), 4096).expect("p3");
        assert_eq!(enc.next_seq(), Seq(2), "the encoder wrapped past 0");
        dec.decode(&p3).expect("d3");
        assert_eq!(dec.last_seq(), Some(Seq(1)));
        dec.decode(&p2).expect("late d2");
        assert_eq!(dec.current(), Some(&expected(enc.schema(), &world(3))));
        assert_eq!(dec.latest_baseline(), Some(Seq(1)));
    }

    /// A new server process starts again at seq 1. Far behind the newest decoded sequence, its
    /// full packet is a restart, not a late packet; `reset()` does the same on purpose.
    #[test]
    fn a_restarted_stream_resets_the_decoder() {
        let (mut enc, mut dec) = pair();
        let mut ack = None;
        for tick in 1..=200u32 {
            let p = enc
                .encode(
                    NetTick(tick),
                    &WorldSnapshot::new(vec![fixed_entity(1, tick)]),
                    ack,
                    4096,
                )
                .expect("old stream");
            ack = Some(dec.decode(&p).expect("old decode").seq);
        }
        let fresh = WorldSnapshot::new(vec![fixed_entity(7, 7)]);
        let mut restarted = SnapshotEncoder::new(test_schema());
        let p = restarted
            .encode(NetTick(1), &fresh, None, 4096)
            .expect("new stream");
        assert_eq!(
            dec.decode(&p).expect("restart").snapshot,
            expected(&test_schema(), &fresh)
        );
        assert_eq!(dec.last_seq(), Some(Seq(1)));
        assert_eq!(dec.baseline_count(), 1, "the old stream's baselines are gone");
        dec.reset();
        assert_eq!(
            (dec.current(), dec.last_seq(), dec.baseline_count()),
            (None, None, 0)
        );
        assert!(!dec.is_rebasing());
        let mut again = SnapshotEncoder::new(test_schema());
        let p = again.encode(NetTick(1), &fresh, None, 4096).expect("after reset");
        assert_eq!(
            dec.decode(&p).expect("decode after reset").snapshot,
            expected(&test_schema(), &fresh)
        );
    }

    /// `is_rebasing()` is true while the shown world holds entities a budget-limited full packet
    /// did not carry, the carried ones show their new values, and the first complete packet
    /// settles the world exactly, including a removal the server made while re-basing.
    #[test]
    fn rebasing_merges_until_a_complete_packet_settles_the_world() {
        let schema = test_schema();
        let mut enc = SnapshotEncoder::with_capacity(schema.clone(), 1);
        let mut dec = SnapshotDecoder::new(schema);
        let mut world = WorldSnapshot::new((1..=60).map(|i| fixed_entity(i, i)).collect());
        let p = enc.encode(NetTick(1), &world, None, 65_536).expect("full");
        dec.decode(&p).expect("full decodes");
        assert!(!dec.is_rebasing());
        // The ack is lost long enough that the one-entry ring no longer holds it.
        enc.encode(NetTick(2), &world, Some(Seq(1)), 65_536).expect("p2");
        world.entities[0].fields[SCORE] = FieldValue::U32(999);
        world.entities.retain(|e| e.id != NetId(60));
        let (p, report) = enc
            .encode_with_report(NetTick(3), &world, Some(Seq(1)), 200)
            .expect("budgeted full");
        assert!(report.is_full && !report.complete);
        let shown = dec.decode(&p).expect("partial full decodes").snapshot;
        assert!(dec.is_rebasing());
        assert_eq!(
            shown.len(),
            60,
            "nothing disappears; entity 60 stays until a complete packet"
        );
        assert_eq!(
            shown.get(NetId(1)).map(|e| e.fields[SCORE]),
            Some(FieldValue::U32(999))
        );
        let mut ack = dec.latest_baseline();
        for tick in 4..100u32 {
            let (p, report) = enc
                .encode_with_report(NetTick(tick), &world, ack, 200)
                .expect("re-base");
            let decoded = dec.decode(&p).expect("re-base decodes");
            ack = dec.latest_baseline();
            if report.complete {
                assert!(!dec.is_rebasing());
                assert_eq!(decoded.snapshot, expected(enc.schema(), &world));
                return;
            }
            assert!(decoded.snapshot.len() >= 59);
        }
        panic!("the stream never completed");
    }

    /// Encode and decode cost for 1 000 entities of the nine-field test schema: one full snapshot,
    /// then deltas with 10 % of entities changing one field. Hand-run:
    /// `cargo test --release -p regia_net --lib snapshot_delta::tests::perf_micro -- --ignored --nocapture`.
    #[test]
    #[ignore = "hand-run timing; run in release with --ignored --nocapture"]
    fn perf_micro_snapshot_delta_1000_entities() {
        use std::time::Instant;

        let schema = test_schema();
        let mut world = random_world(0x5d, 1000);
        let mut rng = Rng::new(0x5d);
        let mut enc = SnapshotEncoder::new(schema.clone());
        let mut dec = SnapshotDecoder::new(schema);
        let start = Instant::now();
        let full = enc.encode(NetTick(0), &world, None, 1 << 20).expect("full");
        let full_encode = start.elapsed();
        let start = Instant::now();
        dec.decode(&full).expect("decode full");
        let full_decode = start.elapsed();
        let rounds = 200u32;
        let mut enc_time = std::time::Duration::ZERO;
        let mut dec_time = std::time::Duration::ZERO;
        let mut bytes = 0usize;
        for tick in 1..=rounds {
            for entity in &mut world.entities {
                if rng.below(10) == 0 {
                    entity.fields[SCORE] = FieldValue::U32(rng.next_u64() as u32);
                }
            }
            let start = Instant::now();
            let packet = enc
                .encode(NetTick(tick), &world, dec.latest_baseline(), 1 << 20)
                .expect("delta");
            enc_time += start.elapsed();
            let start = Instant::now();
            dec.decode(&packet).expect("decode delta");
            dec_time += start.elapsed();
            bytes += packet.len();
        }
        println!(
            "snapshot_delta 1000 entities: full {} bytes, encode {:?}, decode {:?}; \
             delta (10 % changed) mean {} bytes, encode {:?}, decode {:?}",
            full.len(),
            full_encode,
            full_decode,
            bytes / rounds as usize,
            enc_time / rounds,
            dec_time / rounds
        );
    }

    /// Spec 120 § 8.6 (ADR-8186): a deferred entity's update is held back (the client keeps its
    /// acknowledged state, the packet stays complete), a deferred spawn and a removal still go,
    /// the next undeferred encode carries the held change, and a forced unchanged entity gets a
    /// valid record.
    #[test]
    fn deferred_updates_stay_at_the_baseline_and_spawns_still_go() {
        let (mut enc, mut dec) = pair();
        let mut world = random_world(41, 6);
        let p1 = enc.encode(NetTick(1), &world, None, 65_536).expect("full");
        dec.decode(&p1).expect("full decodes");
        let ack = dec.latest_baseline();
        let (a, b) = (world.entities[1].id, world.entities[2].id);
        world.entities[1].fields[SCORE] = FieldValue::U32(9_001);
        world.entities[2].fields[SCORE] = FieldValue::U32(9_002);
        let removed = world.entities.remove(5).id;
        let fresh = random_entity(&mut Rng::new(7), 90_001);
        world.entities.push(fresh.clone());
        // Unsorted on purpose: the encoder sorts a copy.
        let (p2, report) = enc
            .encode_selecting(
                NetTick(2),
                &world,
                ack,
                65_536,
                RecordSelection {
                    defer: &[fresh.id, a],
                    force: &[],
                },
            )
            .expect("deferred delta");
        assert!(report.complete, "a deferral is not a budget omission");
        assert!(!report.included.contains(&a) && !report.omitted.contains(&a));
        assert!(report.included.contains(&b) && report.included.contains(&fresh.id));
        assert!(report.included.contains(&removed), "a removal is never deferred");
        let got = dec.decode(&p2).expect("delta decodes");
        assert_eq!(got.added, vec![fresh.id]);
        assert_eq!(got.removed, vec![removed]);
        assert_eq!(got.updated, vec![b]);
        let shown = |dec: &SnapshotDecoder, id| dec.current().and_then(|w| w.fields_of(id)).map(|f| f[SCORE]);
        assert_ne!(
            shown(&dec, a),
            Some(FieldValue::U32(9_001)),
            "held at the acknowledged state"
        );
        // Undeferred, the held change goes against the newest ack.
        let p3 = enc
            .encode(NetTick(3), &world, dec.latest_baseline(), 65_536)
            .expect("delta");
        let got = dec.decode(&p3).expect("delta decodes");
        assert_eq!(got.updated, vec![a]);
        assert_eq!(got.snapshot, expected(enc.schema(), &world));
        // A forced entity gets a record although nothing changed; the receiver sees it updated
        // and its state stays exact.
        let unchanged = world.entities[0].id;
        let (p4, forced) = enc
            .encode_selecting(
                NetTick(4),
                &world,
                dec.latest_baseline(),
                65_536,
                RecordSelection {
                    defer: &[],
                    force: &[b, unchanged],
                },
            )
            .expect("forced");
        assert!(forced.included.contains(&unchanged) && forced.included.contains(&b));
        let got = dec.decode(&p4).expect("a forced record decodes");
        assert_eq!(got.updated, {
            let mut v = vec![b, unchanged];
            v.sort_unstable();
            v
        });
        assert_eq!(got.snapshot, expected(enc.schema(), &world));
        // A full packet defers nothing.
        let (_, full) = enc
            .encode_selecting(
                NetTick(5),
                &world,
                None,
                65_536,
                RecordSelection {
                    defer: &[a, b],
                    force: &[],
                },
            )
            .expect("full");
        assert!(full.is_full && full.included.contains(&a) && full.included.contains(&b));
    }
}
