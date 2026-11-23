// SPDX-License-Identifier: MPL-2.0
//! Tag names, ids and the process-wide interner.
//!
//! A tag name is a dot-separated path of ASCII segments (`Combat.Damage.Fire`). A [`Tag`] is the
//! interned form: an `Copy` 64-bit id plus the name it was made from, so comparing and hashing
//! cost one integer operation and printing or saving needs no registry.
//!
//! # Id algorithm
//!
//! A tag id is the 64-bit FNV-1a hash of the UTF-8 bytes of the full name (offset basis
//! `0xcbf29ce484222325`, prime `0x100000001b3`, case-sensitive, no separator handling). It is the
//! same in every process, platform and engine version, so an id may be written to a replay or a
//! network packet. The golden values in this module's tests pin it. Two different names that hash
//! alike are refused when the second one is interned ([`TagError::Collision`]); the rename that
//! avoids it is the author's.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::sync::{OnceLock, PoisonError, RwLock};

use regia_app::prelude::*;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Most dot-separated segments in a tag name.
pub const MAX_TAG_DEPTH: usize = 8;

/// Most bytes in one segment.
pub const MAX_SEGMENT_LEN: usize = 48;

/// Most bytes in a whole tag name.
pub const MAX_TAG_NAME_LEN: usize = 192;

/// Why a tag name, table or usage was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TagError {
    /// The name is the empty string.
    #[error("tag name is empty")]
    Empty,
    /// A segment between dots is empty (`A..B`, `.A`, `A.`).
    #[error("tag name `{name}` has an empty segment")]
    EmptySegment {
        /// The offending name.
        name: String,
    },
    /// A character outside `A-Z a-z 0-9 _ -` in a segment.
    #[error(
        "tag name `{name}` has the character {ch:?} at byte {index}; segments use ASCII letters, digits, `_` and `-`"
    )]
    InvalidChar {
        /// The offending name.
        name: String,
        /// The character.
        ch: char,
        /// Its byte offset in the name.
        index: usize,
    },
    /// More than [`MAX_TAG_DEPTH`] segments.
    #[error("tag name `{name}` has {depth} segments; the limit is {max}")]
    TooDeep {
        /// The offending name.
        name: String,
        /// Its segment count.
        depth: usize,
        /// The limit.
        max: usize,
    },
    /// A segment longer than [`MAX_SEGMENT_LEN`] bytes.
    #[error("tag name `{name}` has a segment of {len} bytes; the limit is {max}")]
    SegmentTooLong {
        /// The offending name.
        name: String,
        /// The segment length.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// A name longer than [`MAX_TAG_NAME_LEN`] bytes.
    #[error("tag name is {len} bytes; the limit is {max}")]
    NameTooLong {
        /// The name length.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// The tag is used but the tag table does not declare it.
    #[error("tag `{name}` is not declared in the tag table")]
    Undeclared {
        /// The tag name.
        name: String,
    },
    /// The tag table declares the same name twice.
    #[error("tag `{name}` is declared more than once")]
    Duplicate {
        /// The tag name.
        name: String,
    },
    /// Two different names have the same 64-bit id.
    #[error("tags `{a}` and `{b}` have the same id {id:#018x}; rename one")]
    Collision {
        /// The name interned first.
        a: String,
        /// The name refused.
        b: String,
        /// The shared id.
        id: u64,
    },
    /// A tag table that is not valid RON of the right shape.
    #[error("tag table: {0}")]
    Parse(String),
    /// A tag table written by a newer engine.
    #[error("tag table version {found} is newer than supported version {expected}; upgrade the engine")]
    Version {
        /// Version in the file.
        found: u32,
        /// Version this build reads.
        expected: u32,
    },
    /// A tag table file that could not be read.
    #[error("tag table `{path}`: {message}")]
    Io {
        /// The file.
        path: String,
        /// The operating system's message.
        message: String,
    },
    /// A query nested deeper or larger than the limits.
    #[error("tag query is too large: {0}")]
    QueryTooLarge(String),
}

/// 64-bit FNV-1a of `bytes` (the tag id algorithm, see the module docs).
pub const fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    hash
}

/// Checks a tag name against the naming rules: non-empty, at most [`MAX_TAG_NAME_LEN`] bytes, at
/// most [`MAX_TAG_DEPTH`] non-empty segments of `A-Z a-z 0-9 _ -` each at most
/// [`MAX_SEGMENT_LEN`] bytes. Names are case-sensitive.
pub fn validate_tag_name(name: &str) -> Result<(), TagError> {
    if name.is_empty() {
        return Err(TagError::Empty);
    }
    if name.len() > MAX_TAG_NAME_LEN {
        return Err(TagError::NameTooLong {
            len: name.len(),
            max: MAX_TAG_NAME_LEN,
        });
    }
    let mut depth = 0usize;
    let mut offset = 0usize;
    for segment in name.split('.') {
        depth += 1;
        if segment.is_empty() {
            return Err(TagError::EmptySegment {
                name: name.to_owned(),
            });
        }
        if segment.len() > MAX_SEGMENT_LEN {
            return Err(TagError::SegmentTooLong {
                name: name.to_owned(),
                len: segment.len(),
                max: MAX_SEGMENT_LEN,
            });
        }
        if let Some((at, ch)) = segment
            .char_indices()
            .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_' || *c == '-'))
        {
            return Err(TagError::InvalidChar {
                name: name.to_owned(),
                ch,
                index: offset + at,
            });
        }
        offset += segment.len() + 1;
    }
    if depth > MAX_TAG_DEPTH {
        return Err(TagError::TooDeep {
            name: name.to_owned(),
            depth,
            max: MAX_TAG_DEPTH,
        });
    }
    Ok(())
}

type Interner = RwLock<HashMap<u64, &'static str>>;

fn interner() -> &'static Interner {
    static NAMES: OnceLock<Interner> = OnceLock::new();
    NAMES.get_or_init(|| RwLock::new(HashMap::new()))
}

/// An interned tag: a hierarchical name such as `Combat.Damage.Fire`.
///
/// Equality and hashing use the 64-bit id only. Ordering is by name (so containers list tags
/// alphabetically, the same on every run). The name is kept for the life of the process: each
/// distinct valid name costs one small allocation, once.
///
/// A tag serializes as its name string (`"Combat.Damage.Fire"`), never as the id.
#[derive(Clone, Copy, Reflect)]
#[reflect(opaque)]
#[reflect(Clone, PartialEq, Serialize, Deserialize)]
pub struct Tag {
    id: u64,
    name: &'static str,
}

impl Tag {
    /// Interns `name`, checking it against the naming rules and the id collision rule.
    pub fn new(name: &str) -> Result<Self, TagError> {
        validate_tag_name(name)?;
        let id = fnv1a64(name.as_bytes());
        {
            let names = interner().read().unwrap_or_else(PoisonError::into_inner);
            if let Some(&existing) = names.get(&id) {
                return Self::same_or_collision(id, existing, name);
            }
        }
        let mut names = interner().write().unwrap_or_else(PoisonError::into_inner);
        if let Some(&existing) = names.get(&id) {
            return Self::same_or_collision(id, existing, name);
        }
        let stored: &'static str = Box::leak(name.to_owned().into_boxed_str());
        names.insert(id, stored);
        Ok(Self { id, name: stored })
    }

    fn same_or_collision(id: u64, existing: &'static str, name: &str) -> Result<Self, TagError> {
        if existing == name {
            Ok(Self { id, name: existing })
        } else {
            Err(TagError::Collision {
                a: existing.to_owned(),
                b: name.to_owned(),
                id,
            })
        }
    }

    /// The tag with this id, if a tag of that id has been interned in this process.
    pub fn from_id(id: u64) -> Option<Self> {
        let names = interner().read().unwrap_or_else(PoisonError::into_inner);
        names.get(&id).map(|&name| Self { id, name })
    }

    /// The stable 64-bit id (FNV-1a of the name, see the module docs).
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The full dotted name.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The number of segments (`Combat.Damage.Fire` is 3).
    pub fn depth(&self) -> usize {
        self.name.split('.').count()
    }

    /// The last segment (`Fire`).
    pub fn leaf(&self) -> &'static str {
        self.name.rsplit_once('.').map_or(self.name, |(_, leaf)| leaf)
    }

    /// The parent tag (`Combat.Damage` for `Combat.Damage.Fire`); `None` for a root tag.
    pub fn parent(&self) -> Option<Tag> {
        let (parent, _) = self.name.rsplit_once('.')?;
        Tag::new(parent).ok()
    }

    /// The parent, grandparent, … up to the root, nearest first.
    pub fn ancestors(&self) -> impl Iterator<Item = Tag> {
        std::iter::successors(self.parent(), Tag::parent)
    }

    /// Whether `self` is strictly below `ancestor` in the hierarchy
    /// (`Combat.Damage.Fire` is below `Combat` and `Combat.Damage`, not below itself).
    pub fn is_descendant_of(&self, ancestor: Tag) -> bool {
        self.id != ancestor.id
            && self.name.len() > ancestor.name.len()
            && self.name.starts_with(ancestor.name)
            && self.name.as_bytes()[ancestor.name.len()] == b'.'
    }

    /// Whether `self` is `other` or below it: the match rule of a hierarchical query.
    pub fn matches(&self, other: Tag) -> bool {
        self.id == other.id || self.is_descendant_of(other)
    }
}

impl PartialEq for Tag {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Tag {}

impl std::hash::Hash for Tag {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl PartialOrd for Tag {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Tag {
    fn cmp(&self, other: &Self) -> Ordering {
        if self.id == other.id {
            Ordering::Equal
        } else {
            self.name.cmp(other.name)
        }
    }
}

impl fmt::Debug for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Tag({})", self.name)
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)
    }
}

impl std::str::FromStr for Tag {
    type Err = TagError;

    fn from_str(name: &str) -> Result<Self, TagError> {
        Tag::new(name)
    }
}

impl Serialize for Tag {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name)
    }
}

impl<'de> Deserialize<'de> for Tag {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Tag::new(&name).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a64_matches_the_published_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    /// The ids are part of the format: replays and packets may carry them. Changing one of these
    /// values is a format break.
    #[test]
    fn tag_ids_are_golden() {
        let golden: [(&str, u64); 5] = [
            ("Combat", 0xb274_cc90_a8ac_e101),
            ("Combat.Damage", 0xbfc9_923f_56fb_a788),
            ("Combat.Damage.Fire", 0xec62_c06a_548c_0b46),
            ("Status.Burning", 0x8b1a_8574_0567_c846),
            ("Ability.Jump", 0x3261_4c47_2539_1195),
        ];
        for (name, id) in golden {
            let tag = Tag::new(name).expect("valid name");
            assert_eq!(tag.id(), id, "{name}");
            assert_eq!(Tag::from_id(id), Some(tag));
        }
    }

    #[test]
    fn names_are_validated() {
        for ok in [
            "A",
            "a1",
            "Combat.Damage.Fire",
            "Ability.Jump_High",
            "Mode.third-person",
        ] {
            assert!(validate_tag_name(ok).is_ok(), "{ok}");
        }
        assert_eq!(validate_tag_name(""), Err(TagError::Empty));
        for bad in ["A..B", ".A", "A."] {
            assert!(
                matches!(validate_tag_name(bad), Err(TagError::EmptySegment { .. })),
                "{bad}"
            );
        }
        assert_eq!(
            validate_tag_name("Combat.Dämage"),
            Err(TagError::InvalidChar {
                name: "Combat.Dämage".into(),
                ch: 'ä',
                index: 8
            })
        );
        assert!(matches!(
            validate_tag_name("A B"),
            Err(TagError::InvalidChar {
                ch: ' ',
                index: 1,
                ..
            })
        ));
        let nine = ["S"; MAX_TAG_DEPTH + 1].join(".");
        assert!(matches!(
            validate_tag_name(&nine),
            Err(TagError::TooDeep { depth: 9, .. })
        ));
        let eight = ["S"; MAX_TAG_DEPTH].join(".");
        assert!(validate_tag_name(&eight).is_ok());
        let long_segment = "x".repeat(MAX_SEGMENT_LEN + 1);
        assert!(matches!(
            validate_tag_name(&long_segment),
            Err(TagError::SegmentTooLong { .. })
        ));
        let long_name = vec!["x".repeat(MAX_SEGMENT_LEN); 5].join(".");
        assert!(matches!(
            validate_tag_name(&long_name),
            Err(TagError::NameTooLong { .. })
        ));
    }

    #[test]
    fn hierarchy_helpers() {
        let fire = Tag::new("Combat.Damage.Fire").expect("tag");
        let damage = Tag::new("Combat.Damage").expect("tag");
        let combat = Tag::new("Combat").expect("tag");
        let other = Tag::new("Combat.DamageType").expect("tag");
        assert_eq!(fire.depth(), 3);
        assert_eq!(fire.leaf(), "Fire");
        assert_eq!(fire.parent(), Some(damage));
        assert_eq!(combat.parent(), None);
        assert_eq!(fire.ancestors().collect::<Vec<_>>(), vec![damage, combat]);
        assert!(fire.is_descendant_of(damage));
        assert!(fire.is_descendant_of(combat));
        assert!(!fire.is_descendant_of(fire));
        assert!(!damage.is_descendant_of(fire));
        // A shared name prefix is not a hierarchy relation.
        assert!(!other.is_descendant_of(damage));
        assert!(fire.matches(fire) && fire.matches(damage) && !damage.matches(fire));
    }

    #[test]
    fn interning_is_idempotent_and_ordering_is_by_name() {
        let a = Tag::new("Order.Alpha").expect("tag");
        let again = Tag::new("Order.Alpha").expect("tag");
        let b = Tag::new("Order.Beta").expect("tag");
        assert_eq!(a, again);
        assert_eq!(a.name().as_ptr(), again.name().as_ptr());
        assert!(a < b);
        assert_eq!(a.to_string(), "Order.Alpha");
        assert_eq!("Order.Beta".parse::<Tag>(), Ok(b));
    }

    #[test]
    fn serde_uses_names_not_ids() {
        let tag = Tag::new("Combat.Damage.Fire").expect("tag");
        assert_eq!(
            serde_json::to_string(&tag).expect("json"),
            "\"Combat.Damage.Fire\""
        );
        let back: Tag = serde_json::from_str("\"Combat.Damage.Fire\"").expect("tag");
        assert_eq!(back, tag);
        assert!(serde_json::from_str::<Tag>("\"Bad Name\"").is_err());
    }
}
