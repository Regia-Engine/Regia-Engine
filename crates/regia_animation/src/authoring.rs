// SPDX-License-Identifier: MPL-2.0
//! The `.animedit.ron` authoring sidecar (format v1): what the editor's clip viewer, curve
//! editor and skeleton viewer author on top of imported animation.
//!
//! Clips and skeletons come from glTF import, so editor-authored data — notifies, named float
//! curves, property tracks, a root-motion mode override, sockets and IK chain definitions —
//! lives in one sidecar per source (e.g. `Fox.glb.animedit.ron`). [`AnimEditDocument::apply_to_clip`]
//! and [`AnimEditDocument::apply_to_skeleton`] merge it into the imported assets; curves bake
//! to the runtime's linear tracks ([`Curve::bake`]).
//!
//! Versioned like every frozen format (`docs/specs/12-assets.md` § 4a): a `format_version`
//! field, a migration table ([`animedit_migration_sources`]) and a schema fingerprint hashed by
//! `cargo xtask check-formats`. Reference: `docs/formats/animedit-ron.md`.

use serde::{Deserialize, Serialize};

use crate::clip::{AnimationClip, NamedCurve, Notify, PropertyTrack, RootMotionMode, RootMotionTrack};
use crate::curve::Curve;
use crate::skeleton::{Skeleton, SkeletonSocket};

/// The `.animedit.ron` format version. Version 1 is the first freeze.
pub const ANIMEDIT_FORMAT_VERSION: u32 = 1;

/// A named two-bone IK chain on a skeleton, authored in the skeleton viewer and referenced by
/// `TwoBoneIK` graph nodes (the graph editor fills the node's joints from it).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct IkChainDef {
    /// Chain name (for example `"LeftLeg"`).
    pub name: String,
    /// `[root, mid, tip]` joint names.
    pub joints: [String; 3],
    /// Optional vec3 parameter name the chain's pole should come from.
    pub pole_param: Option<String>,
}

/// A named float curve authored on a clip (becomes a [`NamedCurve`]).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct EditCurve {
    /// Curve name (a graph parameter-like value, e.g. `"FootPlant_L"`).
    pub name: String,
    /// The editable curve.
    pub curve: Curve,
}

/// A property track authored on a clip (becomes a [`PropertyTrack`]).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct EditPropertyTrack {
    /// Reflection path of the animated field (`"Transform.translation.y"`).
    pub path: String,
    /// The editable curve.
    pub curve: Curve,
}

/// Everything authored on one clip.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ClipEdits {
    /// Clip name (matches [`AnimationClip::name`]).
    pub clip: String,
    /// Notifies; replace the imported clip's notifies when applied.
    pub notifies: Vec<Notify>,
    /// Named float curves; added or replaced by name.
    pub curves: Vec<EditCurve>,
    /// Property tracks; added or replaced by path.
    pub property_tracks: Vec<EditPropertyTrack>,
    /// Root-motion extraction mode override (`None` keeps the import's).
    pub root_motion: Option<RootMotionMode>,
}

/// The on-disk document.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AnimEditDocument {
    /// Format version of this file.
    pub format_version: u32,
    /// Sockets added or replaced (by name) on the skeleton.
    pub sockets: Vec<SkeletonSocket>,
    /// IK chains defined on the skeleton.
    pub ik_chains: Vec<IkChainDef>,
    /// Per-clip edits.
    pub clips: Vec<ClipEdits>,
}

impl Default for AnimEditDocument {
    fn default() -> Self {
        Self {
            format_version: ANIMEDIT_FORMAT_VERSION,
            sockets: Vec::new(),
            ik_chains: Vec::new(),
            clips: Vec::new(),
        }
    }
}

/// Errors loading an `.animedit.ron`.
#[derive(Debug, thiserror::Error)]
pub enum AnimEditError {
    /// RON parse error.
    #[error("animedit RON parse error: {0}")]
    RonParse(#[from] ron::error::SpannedError),
    /// RON write error.
    #[error("animedit RON write error: {0}")]
    RonWrite(#[from] ron::Error),
    /// No migration reaches the current version.
    #[error(
        "animedit format version {0} is unknown (current is {ANIMEDIT_FORMAT_VERSION}); no migration reaches it"
    )]
    UnknownVersion(u32),
    /// The document is malformed (missing version field, bad data).
    #[error("invalid animedit document: {0}")]
    Invalid(String),
}

/// One migration step `from → from + 1`.
#[derive(Clone, Copy)]
pub struct AnimEditMigration {
    /// Version read.
    pub from: u32,
    /// Applies the transformation in place.
    pub apply: fn(&mut AnimEditDocument) -> Result<(), String>,
}

/// Built-in migrations (empty: v1 is the first version).
#[must_use]
pub fn animedit_migrations() -> &'static [AnimEditMigration] {
    &[]
}

/// `from` versions of the registered migrations (for `cargo xtask check-formats`).
#[must_use]
pub fn animedit_migration_sources() -> Vec<u32> {
    animedit_migrations().iter().map(|m| m.from).collect()
}

/// The serde field-list fingerprint of the format (every nested struct and enum).
#[must_use]
pub fn animedit_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<AnimEditDocument>(),
        serde_field_names::<IkChainDef>(),
        serde_field_names::<ClipEdits>(),
        serde_field_names::<EditCurve>(),
        serde_field_names::<EditPropertyTrack>(),
        serde_field_names::<Curve>(),
        serde_field_names::<crate::curve::CurveKey>(),
        serde_variant_names::<crate::curve::CurveKeyKind>(),
        serde_field_names::<Notify>(),
        serde_field_names::<SkeletonSocket>(),
        serde_field_names::<crate::skeleton::LocalBind>(),
        serde_variant_names::<RootMotionMode>(),
    ])
}

impl AnimEditDocument {
    /// Parses `.animedit.ron` text: peeks the version (newer is rejected even when the body no
    /// longer parses), runs migrations, returns the current-version document.
    ///
    /// # Errors
    ///
    /// Syntax errors, unknown versions, failed migrations.
    pub fn from_ron_str(text: &str) -> Result<Self, AnimEditError> {
        let version = regia_core::version::peek_ron_version(text)
            .map_err(|e| AnimEditError::Invalid(format!("cannot read format_version: {e}")))?;
        if version > ANIMEDIT_FORMAT_VERSION {
            return Err(AnimEditError::UnknownVersion(version));
        }
        let mut document: AnimEditDocument = ron::from_str(text)?;
        migrate_animedit(&mut document, animedit_migrations())?;
        Ok(document)
    }

    /// Serialises at the current version.
    ///
    /// # Errors
    ///
    /// Serialisation failure.
    pub fn to_ron_string(&self) -> Result<String, AnimEditError> {
        let mut document = self.clone();
        document.format_version = ANIMEDIT_FORMAT_VERSION;
        let pretty = regia_core::ron_pretty();
        Ok(ron::ser::to_string_pretty(&document, pretty)?)
    }

    /// The edits of `clip`, if any.
    #[must_use]
    pub fn clip(&self, clip: &str) -> Option<&ClipEdits> {
        self.clips.iter().find(|c| c.clip == clip)
    }

    /// The edits of `clip`, created empty when absent.
    pub fn clip_mut(&mut self, clip: &str) -> &mut ClipEdits {
        if let Some(index) = self.clips.iter().position(|c| c.clip == clip) {
            return &mut self.clips[index];
        }
        self.clips.push(ClipEdits {
            clip: clip.to_string(),
            ..ClipEdits::default()
        });
        let last = self.clips.len() - 1;
        &mut self.clips[last]
    }

    /// The IK chain called `name`.
    #[must_use]
    pub fn ik_chain(&self, name: &str) -> Option<&IkChainDef> {
        self.ik_chains.iter().find(|c| c.name == name)
    }

    /// Merges this document's edits of `clip.name` into `clip`. Returns whether anything applied.
    pub fn apply_to_clip(&self, clip: &mut AnimationClip) -> bool {
        let Some(edits) = self.clip(&clip.name) else {
            return false;
        };
        let mut notifies = edits.notifies.clone();
        notifies.sort_by(|a, b| a.time.total_cmp(&b.time));
        clip.notifies = notifies;
        let rate = clip.sample_rate;
        let duration = clip.duration;
        for edit in &edits.curves {
            let baked = NamedCurve {
                name: edit.name.clone(),
                curve: edit.curve.bake(rate, duration),
            };
            match clip.curves.iter_mut().find(|c| c.name == edit.name) {
                Some(existing) => *existing = baked,
                None => clip.curves.push(baked),
            }
        }
        for edit in &edits.property_tracks {
            let baked = PropertyTrack {
                path: edit.path.clone(),
                curve: edit.curve.bake(rate, duration),
            };
            match clip.property_tracks.iter_mut().find(|t| t.path == edit.path) {
                Some(existing) => *existing = baked,
                None => clip.property_tracks.push(baked),
            }
        }
        if let Some(mode) = edits.root_motion {
            match &mut clip.root_motion {
                Some(track) => track.mode = mode,
                None if mode != RootMotionMode::None => {
                    clip.root_motion = Some(RootMotionTrack { joint: 0, mode });
                }
                None => {}
            }
        }
        true
    }

    /// Adds or replaces (by name) this document's sockets on `skeleton`. Sockets on joints the
    /// skeleton lacks are skipped and returned.
    pub fn apply_to_skeleton(&self, skeleton: &mut Skeleton) -> Vec<String> {
        let mut skipped = Vec::new();
        for socket in &self.sockets {
            if skeleton.joint_index(&socket.joint).is_none() {
                skipped.push(socket.name.clone());
                continue;
            }
            match skeleton.sockets.iter_mut().find(|s| s.name == socket.name) {
                Some(existing) => *existing = socket.clone(),
                None => skeleton.sockets.push(socket.clone()),
            }
        }
        skipped
    }
}

/// Runs `steps` until the document reaches [`ANIMEDIT_FORMAT_VERSION`].
///
/// # Errors
///
/// [`AnimEditError::UnknownVersion`] when the chain is incomplete.
pub fn migrate_animedit(
    document: &mut AnimEditDocument,
    steps: &[AnimEditMigration],
) -> Result<u32, AnimEditError> {
    let mut applied = 0;
    while document.format_version < ANIMEDIT_FORMAT_VERSION {
        let Some(step) = steps.iter().find(|s| s.from == document.format_version) else {
            return Err(AnimEditError::UnknownVersion(document.format_version));
        };
        (step.apply)(document).map_err(|e| {
            AnimEditError::Invalid(format!("migration {}→{} failed: {e}", step.from, step.from + 1))
        })?;
        document.format_version = step.from + 1;
        applied += 1;
    }
    if document.format_version > ANIMEDIT_FORMAT_VERSION {
        return Err(AnimEditError::UnknownVersion(document.format_version));
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curve::{CurveKey, CurveKeyKind};
    use crate::skeleton::LocalBind;

    fn sample() -> AnimEditDocument {
        let mut doc = AnimEditDocument::default();
        doc.sockets.push(SkeletonSocket {
            name: "mouth".into(),
            joint: "head".into(),
            offset: LocalBind::default(),
        });
        doc.ik_chains.push(IkChainDef {
            name: "Leg".into(),
            joints: ["hip".into(), "knee".into(), "foot".into()],
            pole_param: Some("LegPole".into()),
        });
        let walk = doc.clip_mut("Walk");
        walk.notifies.push(Notify {
            time: 0.6,
            name: "Footstep_R".into(),
            payload: None,
        });
        walk.notifies.push(Notify {
            time: 0.1,
            name: "Footstep_L".into(),
            payload: Some("(volume: 0.8)".into()),
        });
        walk.curves.push(EditCurve {
            name: "FootPlant_L".into(),
            curve: Curve::from(vec![
                CurveKey::new(0.0, 1.0, CurveKeyKind::Linear),
                CurveKey::new(0.5, 0.0, CurveKeyKind::Linear),
            ]),
        });
        walk.property_tracks.push(EditPropertyTrack {
            path: "Transform.translation.y".into(),
            curve: Curve::from(vec![CurveKey::new(0.0, 0.25, CurveKeyKind::Constant)]),
        });
        walk.root_motion = Some(RootMotionMode::XZ);
        doc
    }

    #[test]
    fn round_trips_and_rejects_newer_versions() {
        let doc = sample();
        let text = doc.to_ron_string().expect("write");
        assert!(text.contains("format_version: 1"));
        let back = AnimEditDocument::from_ron_str(&text).expect("parse");
        assert_eq!(back, doc);
        let newer = text.replace("format_version: 1", "format_version: 7");
        assert!(matches!(
            AnimEditDocument::from_ron_str(&newer),
            Err(AnimEditError::UnknownVersion(7))
        ));
        assert!(AnimEditDocument::from_ron_str("(nonsense").is_err());
    }

    #[test]
    fn migration_chain_runs_and_reports_gaps() {
        let mut doc = sample();
        doc.format_version = 0;
        let steps = [AnimEditMigration {
            from: 0,
            apply: |d| {
                d.ik_chains.clear();
                Ok(())
            },
        }];
        assert_eq!(migrate_animedit(&mut doc, &steps).expect("migrates"), 1);
        assert!(doc.ik_chains.is_empty());
        assert_eq!(doc.format_version, ANIMEDIT_FORMAT_VERSION);
        let mut gap = sample();
        gap.format_version = 0;
        assert!(matches!(
            migrate_animedit(&mut gap, &[]),
            Err(AnimEditError::UnknownVersion(0))
        ));
    }

    #[test]
    fn applies_to_clips_and_skeletons() {
        let doc = sample();
        let mut clip = AnimationClip::empty("Walk", 1.0);
        assert!(doc.apply_to_clip(&mut clip));
        assert_eq!(clip.notifies.len(), 2);
        assert_eq!(clip.notifies[0].name, "Footstep_L", "sorted by time");
        assert_eq!(clip.curves.len(), 1);
        assert!((clip.curves[0].curve.sample(0.25) - 0.5).abs() < 1e-3);
        assert_eq!(clip.property_tracks[0].curve.sample(0.7), 0.25);
        assert_eq!(
            clip.root_motion.as_ref().map(|r| r.mode),
            Some(RootMotionMode::XZ)
        );
        // Applying twice replaces instead of duplicating.
        assert!(doc.apply_to_clip(&mut clip));
        assert_eq!(clip.curves.len(), 1);
        assert_eq!(clip.property_tracks.len(), 1);
        // Other clips are untouched.
        let mut run = AnimationClip::empty("Run", 1.0);
        assert!(!doc.apply_to_clip(&mut run));

        let mut skeleton = Skeleton::from_local_bind(
            vec!["root".into(), "head".into()],
            vec![None, Some(0)],
            vec![LocalBind::default(); 2],
        )
        .expect("skeleton");
        let mut with_missing = doc.clone();
        with_missing.sockets.push(SkeletonSocket {
            name: "tail_tip".into(),
            joint: "tail".into(),
            offset: LocalBind::default(),
        });
        let skipped = with_missing.apply_to_skeleton(&mut skeleton);
        assert_eq!(skipped, vec!["tail_tip".to_string()]);
        assert_eq!(skeleton.sockets.len(), 1);
        assert_eq!(doc.ik_chain("Leg").map(|c| c.joints[1].as_str()), Some("knee"));
    }

    #[test]
    fn loads_the_published_format_example() {
        let doc = include_str!("../../../docs/formats/animedit-ron.md");
        let start = doc.find("```ron\n").map(|i| i + 7).expect("ron fence");
        let end = doc[start..].find("\n```").map(|i| start + i).expect("fence end");
        let parsed = AnimEditDocument::from_ron_str(&doc[start..end]).expect("example parses");
        assert_eq!(parsed.ik_chains[0].joints[1], "shin_l");
        assert_eq!(parsed.clip("Walk").map(|c| c.notifies.len()), Some(2));
    }

    #[test]
    fn fingerprint_lists_every_struct() {
        let fp = animedit_schema_fingerprint();
        for field in ["ik_chains", "pole_param", "property_tracks", "Cubic", "payload"] {
            assert!(fp.contains(field), "{field} missing from {fp}");
        }
        assert!(animedit_migration_sources().is_empty());
    }
}
