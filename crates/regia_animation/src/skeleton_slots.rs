// SPDX-License-Identifier: MPL-2.0
//! The skeleton-owned slot table (`.slots.ron`, frozen v1).
//!
//! Specification: `docs/specs/34-montage-extensions.md` § 3 (M1). Every rig of one skeleton
//! offers the same slots: a [`SkeletonSlots`] lists each slot's name, group and joint mask, so
//! a montage that names a slot the skeleton lacks is found when the table is checked
//! ([`Montage::validate_against`]) rather than at play time. A missing table is the engine
//! default (one full-body slot `default` in group `default`), which is what an undefined slot
//! already was.
//!
//! Editing the table is a pure operation over the montages that name a slot: rename rewrites
//! every referencing montage or none, delete lists the referencers and is refused while any
//! exist unless forced. Reading and writing the files is the caller's job (the editor and the
//! agent protocol), so the all-or-none rule holds over in-memory documents here.

use serde::{Deserialize, Serialize};

use crate::graph::JointMaskDef;
use crate::montage::Montage;
use crate::montage_player::{DEFAULT_SLOT_GROUP, SlotDef};
use crate::skeleton::Skeleton;

/// The `.slots.ron` format version (v1 is the first version).
pub const SLOTS_FORMAT_VERSION: u32 = 1;

/// The longest slot name, characters.
pub const MAX_SLOT_NAME: usize = 48;

/// One slot of the table.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SlotEntry {
    /// Slot name: unique, lowercase ASCII, digits and `_`, 1..=48 characters.
    pub name: String,
    /// Slot group: a montage interrupts the playing montage whose main slot shares its group.
    pub group: String,
    /// Joints the slot covers; `None` = the whole body.
    #[serde(default)]
    pub mask: Option<JointMaskDef>,
}

/// The `.slots.ron` document: the slots a skeleton offers.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SkeletonSlots {
    /// Format version of this file.
    #[serde(default)]
    pub format_version: u32,
    /// The slots.
    pub slots: Vec<SlotEntry>,
}

/// Errors reading, validating or editing a slot table.
#[derive(Debug, thiserror::Error)]
pub enum SlotsError {
    /// RON syntax or shape error.
    #[error("slots RON: {0}")]
    Ron(#[from] ron::error::SpannedError),
    /// RON write error.
    #[error("slots RON write: {0}")]
    RonWrite(#[from] ron::Error),
    /// The file is newer than this engine.
    #[error("slots format version {0} is unknown (current is {SLOTS_FORMAT_VERSION})")]
    UnknownVersion(u32),
    /// The table is malformed.
    #[error("invalid slot table: {0}")]
    Invalid(String),
    /// A mask names a joint the skeleton lacks.
    #[error("slot {slot:?}: mask joint {joint:?} is not in the skeleton")]
    UnknownJoint {
        /// The slot.
        slot: String,
        /// The missing joint.
        joint: String,
    },
    /// An edit named a slot the table does not have (`slot_unknown`).
    #[error("slot_unknown: no slot named {0:?}")]
    UnknownSlot(String),
    /// A delete or rename target is still used by montages (`slot_in_use`).
    #[error("slot_in_use: slot {slot:?} is named by montages {montages:?}")]
    InUse {
        /// The slot.
        slot: String,
        /// Names of the montages that name it.
        montages: Vec<String>,
    },
}

/// `from` versions of the registered migrations (none: v1 is the first version).
#[must_use]
pub fn slots_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde field-list fingerprint of the format (for `cargo xtask check-formats`).
#[must_use]
pub fn slots_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names};
    join_field_lists(&[
        serde_field_names::<SkeletonSlots>(),
        serde_field_names::<SlotEntry>(),
        serde_field_names::<JointMaskDef>(),
    ])
}

/// A problem a montage has against a slot table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MontageIssue {
    /// Stable lowercase code (`montage_unknown_slot`).
    pub code: &'static str,
    /// Track index the issue is on.
    pub track: usize,
    /// Human-readable detail.
    pub detail: String,
}

/// Whether `name` is a valid slot name.
#[must_use]
pub fn is_valid_slot_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SLOT_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

impl Default for SkeletonSlots {
    /// The engine default table: one full-body slot `default` in group `default`.
    fn default() -> Self {
        SkeletonSlots {
            format_version: SLOTS_FORMAT_VERSION,
            slots: vec![SlotEntry {
                name: "default".into(),
                group: DEFAULT_SLOT_GROUP.into(),
                mask: None,
            }],
        }
    }
}

impl SkeletonSlots {
    /// Parses `.slots.ron` text: peeks the version (newer is rejected), validates.
    ///
    /// # Errors
    ///
    /// Syntax errors, unknown versions and failed [`SkeletonSlots::validate`].
    pub fn from_ron_str(text: &str) -> Result<Self, SlotsError> {
        let version = regia_core::version::peek_ron_version(text)
            .map_err(|e| SlotsError::Invalid(format!("cannot read format_version: {e}")))?;
        if version > SLOTS_FORMAT_VERSION {
            return Err(SlotsError::UnknownVersion(version));
        }
        let mut table: SkeletonSlots = ron::from_str(text)?;
        table.format_version = SLOTS_FORMAT_VERSION;
        table.validate()?;
        Ok(table)
    }

    /// Serializes at the current version.
    ///
    /// # Errors
    ///
    /// Serialization failure.
    pub fn to_ron_string(&self) -> Result<String, SlotsError> {
        let mut table = self.clone();
        table.format_version = SLOTS_FORMAT_VERSION;
        let pretty = regia_core::ron_pretty();
        Ok(ron::ser::to_string_pretty(&table, pretty)?)
    }

    /// Checks names (valid and unique) and that every slot has a group.
    ///
    /// # Errors
    ///
    /// The first problem found.
    pub fn validate(&self) -> Result<(), SlotsError> {
        for (i, slot) in self.slots.iter().enumerate() {
            if !is_valid_slot_name(&slot.name) {
                return Err(SlotsError::Invalid(format!(
                    "slot name {:?} must be 1..={MAX_SLOT_NAME} characters of a-z, 0-9 and _",
                    slot.name
                )));
            }
            if self.slots[..i].iter().any(|s| s.name == slot.name) {
                return Err(SlotsError::Invalid(format!(
                    "two slots are named {:?}",
                    slot.name
                )));
            }
            if slot.group.is_empty() {
                return Err(SlotsError::Invalid(format!("slot {:?} has no group", slot.name)));
            }
        }
        Ok(())
    }

    /// Checks every mask joint exists in `skeleton`.
    ///
    /// # Errors
    ///
    /// [`SlotsError::UnknownJoint`] naming the first missing joint.
    pub fn validate_against(&self, skeleton: &Skeleton) -> Result<(), SlotsError> {
        for slot in &self.slots {
            let Some(mask) = &slot.mask else {
                continue;
            };
            if let Some(joint) = mask
                .joints
                .iter()
                .find(|j| !skeleton.joint_names.iter().any(|n| n == *j))
            {
                return Err(SlotsError::UnknownJoint {
                    slot: slot.name.clone(),
                    joint: joint.clone(),
                });
            }
        }
        Ok(())
    }

    /// The slot called `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&SlotEntry> {
        self.slots.iter().find(|s| s.name == name)
    }

    /// The distinct slot groups, in table order.
    #[must_use]
    pub fn groups(&self) -> Vec<&str> {
        let mut groups: Vec<&str> = Vec::new();
        for slot in &self.slots {
            if !groups.contains(&slot.group.as_str()) {
                groups.push(&slot.group);
            }
        }
        groups
    }

    /// The table as player slot definitions (for `MontagePlayer::new` or its `slots` field).
    #[must_use]
    pub fn slot_defs(&self) -> Vec<SlotDef> {
        self.slots
            .iter()
            .map(|s| SlotDef {
                name: s.name.clone(),
                group: s.group.clone(),
                mask: s.mask.clone(),
            })
            .collect()
    }

    /// Names of the montages whose tracks play slot `name`.
    #[must_use]
    pub fn referencing<'a>(name: &str, montages: &'a [Montage]) -> Vec<&'a str> {
        montages
            .iter()
            .filter(|m| m.tracks.iter().any(|t| t.slot == name))
            .map(|m| m.name.as_str())
            .collect()
    }

    /// Renames slot `old` to `new` and rewrites every montage track that plays it: all or none.
    ///
    /// # Errors
    ///
    /// [`SlotsError::UnknownSlot`] when `old` is absent; [`SlotsError::Invalid`] when `new` is
    /// not a valid name or is taken, or when a referencing montage would end up with two tracks
    /// on one slot. Nothing is changed on error.
    pub fn rename(&mut self, old: &str, new: &str, montages: &mut [Montage]) -> Result<(), SlotsError> {
        if self.get(old).is_none() {
            return Err(SlotsError::UnknownSlot(old.into()));
        }
        if !is_valid_slot_name(new) {
            return Err(SlotsError::Invalid(format!(
                "slot name {new:?} must be 1..={MAX_SLOT_NAME} characters of a-z, 0-9 and _"
            )));
        }
        if old != new && self.get(new).is_some() {
            return Err(SlotsError::Invalid(format!("a slot named {new:?} exists")));
        }
        // Check every montage first so a late failure cannot leave a half-rewrite.
        for montage in montages.iter() {
            let plays_old = montage.tracks.iter().any(|t| t.slot == old);
            let plays_new = montage.tracks.iter().any(|t| t.slot == new);
            if old != new && plays_old && plays_new {
                return Err(SlotsError::Invalid(format!(
                    "montage {:?} already plays slot {new:?}",
                    montage.name
                )));
            }
        }
        for montage in montages.iter_mut() {
            for track in &mut montage.tracks {
                if track.slot == old {
                    track.slot = new.into();
                }
            }
        }
        if let Some(slot) = self.slots.iter_mut().find(|s| s.name == old) {
            slot.name = new.into();
        }
        Ok(())
    }

    /// Deletes slot `name`. Refused while a montage plays it unless `force`.
    ///
    /// # Errors
    ///
    /// [`SlotsError::UnknownSlot`], or [`SlotsError::InUse`] listing the referencing montages.
    pub fn delete(&mut self, name: &str, force: bool, montages: &[Montage]) -> Result<(), SlotsError> {
        if self.get(name).is_none() {
            return Err(SlotsError::UnknownSlot(name.into()));
        }
        let users = Self::referencing(name, montages);
        if !users.is_empty() && !force {
            return Err(SlotsError::InUse {
                slot: name.into(),
                montages: users.into_iter().map(str::to_owned).collect(),
            });
        }
        self.slots.retain(|s| s.name != name);
        Ok(())
    }
}

/// The cook step's slot check (spec 34 M1, ADR-7216): a montage track whose slot no slot table
/// of the project names is an error (`montage_unknown_slot`). A montage file names no skeleton,
/// so the check is against every `.slots.ron` the project has; a project with none defines its
/// slots in code (or uses the engine default) and nothing is checked.
#[must_use]
pub fn cook_slot_errors(montage: &Montage, tables: &[SkeletonSlots]) -> Vec<MontageIssue> {
    if tables.is_empty() {
        return Vec::new();
    }
    montage
        .tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| tables.iter().all(|table| table.get(&t.slot).is_none()))
        .map(|(track, t)| MontageIssue {
            code: "montage_unknown_slot",
            track,
            detail: format!(
                "montage {:?} track {track} plays slot {:?}, which no slot table of the project names",
                montage.name, t.slot
            ),
        })
        .collect()
}

impl Montage {
    /// Checks every track's slot against `slots`: one `montage_unknown_slot` issue per track
    /// naming a slot the table lacks. At load this is a warning (the track plays full body in
    /// group `default`); the cook step treats it as an error.
    #[must_use]
    pub fn validate_against(&self, slots: &SkeletonSlots) -> Vec<MontageIssue> {
        self.tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| slots.get(&t.slot).is_none())
            .map(|(track, t)| MontageIssue {
                code: "montage_unknown_slot",
                track,
                detail: format!(
                    "montage {:?} track {track} plays unknown slot {:?}",
                    self.name, t.slot
                ),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> SkeletonSlots {
        SkeletonSlots {
            format_version: SLOTS_FORMAT_VERSION,
            slots: vec![
                SlotEntry {
                    name: "upper_body".into(),
                    group: "upper".into(),
                    mask: Some(JointMaskDef {
                        joints: vec!["spine".into()],
                        include_children: true,
                    }),
                },
                SlotEntry {
                    name: "full".into(),
                    group: "default".into(),
                    mask: None,
                },
            ],
        }
    }

    fn montage(name: &str, slot: &str) -> Montage {
        Montage::single(name, slot, "clip")
    }

    #[test]
    fn the_table_round_trips() {
        let text = table().to_ron_string().expect("write");
        let back = SkeletonSlots::from_ron_str(&text).expect("read");
        assert_eq!(back, table());
        assert_eq!(back.groups(), vec!["upper", "default"]);
        assert_eq!(back.slot_defs().len(), 2);
    }

    #[test]
    fn bad_names_and_duplicates_are_refused() {
        for bad in ["", "Upper", "a b", &"x".repeat(49)] {
            let mut t = table();
            t.slots[0].name = bad.into();
            assert!(t.validate().is_err(), "{bad:?}");
        }
        let mut t = table();
        t.slots[1].name = "upper_body".into();
        assert!(t.validate().is_err());
        assert!(SkeletonSlots::default().validate().is_ok());
    }

    #[test]
    fn a_newer_version_is_refused() {
        let err = SkeletonSlots::from_ron_str("(format_version: 9, slots: [])");
        assert!(matches!(err, Err(SlotsError::UnknownVersion(9))));
    }

    #[test]
    fn a_mask_joint_the_skeleton_lacks_is_named() {
        let skeleton = Skeleton {
            joint_names: vec!["root".into(), "hips".into()],
            parents: vec![None, Some(0)],
            ..Skeleton::default()
        };
        match table().validate_against(&skeleton) {
            Err(SlotsError::UnknownJoint { slot, joint }) => {
                assert_eq!((slot.as_str(), joint.as_str()), ("upper_body", "spine"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_montage_with_an_unknown_slot_reports_the_track() {
        let m = montage("swing", "tail");
        let issues = m.validate_against(&table());
        assert_eq!(issues.len(), 1);
        assert_eq!((issues[0].code, issues[0].track), ("montage_unknown_slot", 0));
        assert!(montage("ok", "full").validate_against(&table()).is_empty());
    }

    #[test]
    fn rename_rewrites_every_referencing_montage() {
        let mut t = table();
        let mut ms = vec![
            montage("a", "upper_body"),
            montage("b", "upper_body"),
            montage("c", "full"),
        ];
        t.rename("upper_body", "torso", &mut ms).expect("rename");
        assert!(t.get("torso").is_some() && t.get("upper_body").is_none());
        assert_eq!(ms[0].tracks[0].slot, "torso");
        assert_eq!(ms[1].tracks[0].slot, "torso");
        assert_eq!(ms[2].tracks[0].slot, "full");
    }

    #[test]
    fn a_failing_rename_changes_nothing() {
        let mut t = table();
        let mut both = Montage::single("both", "upper_body", "clip");
        both.tracks.push(crate::montage::MontageTrack {
            slot: "full".into(),
            segments: both.tracks[0].segments.clone(),
        });
        let mut ms = vec![montage("a", "upper_body"), both];
        let before = ms.clone();
        let err = t.rename("upper_body", "full", &mut ms);
        assert!(err.is_err());
        assert_eq!(ms, before);
        assert!(t.get("upper_body").is_some());
        assert!(matches!(
            t.rename("nope", "x", &mut ms),
            Err(SlotsError::UnknownSlot(_))
        ));
        assert!(t.rename("upper_body", "Bad Name", &mut ms).is_err());
    }

    #[test]
    fn delete_lists_referencers_and_force_overrides() {
        let mut t = table();
        let ms = vec![montage("a", "upper_body"), montage("c", "full")];
        match t.delete("upper_body", false, &ms) {
            Err(SlotsError::InUse { montages, .. }) => assert_eq!(montages, vec!["a".to_string()]),
            other => panic!("{other:?}"),
        }
        assert!(t.get("upper_body").is_some());
        t.delete("upper_body", true, &ms).expect("forced delete");
        assert!(t.get("upper_body").is_none());
        t.delete("full", false, &[]).expect("unused delete");
    }
}
