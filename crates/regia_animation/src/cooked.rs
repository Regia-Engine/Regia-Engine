// SPDX-License-Identifier: MPL-2.0
//! Cooked asset loaders for Skeleton and AnimationClip (`docs/specs/12-assets.md` § 3).
//!
//! These loaders deserialize postcard payloads from cooked `.fckd` files produced by
//! `regia_asset_pipeline`. They are registered in the runtime and do not depend on
//! the pipeline crate.

use regia_asset::{AssetLoadError, CookedHeader, decode_cooked_file};
use regia_core::id::TypePathHash;

use crate::clip::AnimationClip;
use crate::skeleton::Skeleton;

/// Type tag for a cooked [`Skeleton`].
pub fn skeleton_type_tag() -> TypePathHash {
    TypePathHash::of("regia_animation::Skeleton")
}

/// Type tag for a cooked [`AnimationClip`].
pub fn clip_type_tag() -> TypePathHash {
    TypePathHash::of("regia_animation::AnimationClip")
}

/// Type tag for a cooked motion-matching database
/// ([`crate::motion_matching::MotionDatabase`], `docs/specs/52-motion-matching.md` § 4).
pub fn motion_database_type_tag() -> TypePathHash {
    TypePathHash::of("regia_animation::MotionDatabase")
}

/// Why a cooked motion database did not load.
#[derive(Debug, thiserror::Error)]
pub enum MotionDatabaseLoadError {
    /// The container (header, type tag).
    #[error(transparent)]
    Asset(#[from] AssetLoadError),
    /// The payload (version, hash, shape, sample rows).
    #[error(transparent)]
    Database(#[from] crate::motion_matching::MotionMatchError),
}

/// Load a motion-matching database from cooked file bytes.
///
/// # Errors
///
/// [`MotionDatabaseLoadError::Asset`] for a malformed header or another type tag,
/// [`MotionDatabaseLoadError::Database`] for a payload that fails its checks.
pub fn load_cooked_motion_database(
    bytes: &[u8],
) -> Result<(CookedHeader, crate::motion_matching::MotionDatabase), MotionDatabaseLoadError> {
    let (header, payload) = decode_cooked_file(bytes).map_err(AssetLoadError::from)?;
    if header.type_tag != motion_database_type_tag() {
        return Err(AssetLoadError::TypeMismatch {
            id: header.asset_id,
            found: header.type_tag.get(),
            expected: motion_database_type_tag().get(),
        }
        .into());
    }
    let database = crate::motion_matching::MotionDatabase::from_cooked_payload(payload)?;
    Ok((header, database))
}

/// The payload version of a cooked motion-database file (`regia upgrade-project`'s probe):
/// `None` for another cooked type.
///
/// # Errors
///
/// Returns the failure as text when the container or the payload's hash does not check.
pub fn cooked_motion_database_file_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let (header, payload) = decode_cooked_file(bytes).map_err(|e| e.to_string())?;
    if header.type_tag != motion_database_type_tag() {
        return Ok(None);
    }
    crate::motion_matching::MotionDatabase::cooked_payload_version(payload)
        .map(Some)
        .map_err(|e| e.to_string())
}

/// Rewrites a cooked motion-database file at the current payload version with its header kept
/// (`regia upgrade-project`, ADR-7817; [`crate::motion_matching::migrate_cooked_motiondb`]):
/// the same database plus the per-frame clip-speed array.
///
/// # Errors
///
/// Returns the failure as text when the file is not a cooked motion database or does not load.
pub fn migrate_cooked_motion_database_file(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let (mut header, payload) = decode_cooked_file(bytes).map_err(|e| e.to_string())?;
    if header.type_tag != motion_database_type_tag() {
        return Err("not a cooked motion database".to_string());
    }
    let out = crate::motion_matching::migrate_cooked_motiondb(payload).map_err(|e| e.to_string())?;
    header.payload_len = out.len() as u64;
    regia_asset::encode_cooked_file(&header, &out).map_err(|e| e.to_string())
}

/// Load a [`Skeleton`] from cooked file bytes.
///
/// # Errors
///
/// Returns [`AssetLoadError`] when the header is malformed, the type tag mismatches, or the
/// payload cannot be deserialized or fails [`Skeleton::validate`]: everything downstream indexes
/// by joint and walks parent links on the strength of that check.
pub fn load_cooked_skeleton(bytes: &[u8]) -> Result<(CookedHeader, Skeleton), AssetLoadError> {
    let (header, payload) = decode_cooked_file(bytes)?;
    if header.type_tag != skeleton_type_tag() {
        return Err(AssetLoadError::TypeMismatch {
            id: header.asset_id,
            found: header.type_tag.get(),
            expected: skeleton_type_tag().get(),
        });
    }
    let skeleton: Skeleton = postcard::from_bytes(payload)?;
    skeleton
        .validate()
        .map_err(|e| AssetLoadError::Invalid(format!("invalid skeleton: {e}")))?;
    Ok((header, skeleton))
}

/// Load an [`AnimationClip`] from cooked file bytes.
///
/// # Errors
///
/// Returns [`AssetLoadError`] when the header is malformed, the type tag mismatches, or the
/// payload cannot be deserialized. A payload stamped with a version above
/// [`crate::clip::CLIP_FORMAT_VERSION`] is reported as such (the payload decoder alone would
/// answer only with postcard's opaque "bad encoding" text).
pub fn load_cooked_clip(bytes: &[u8]) -> Result<(CookedHeader, AnimationClip), AssetLoadError> {
    let (header, payload) = decode_cooked_file(bytes)?;
    if header.type_tag != clip_type_tag() {
        return Err(AssetLoadError::TypeMismatch {
            id: header.asset_id,
            found: header.type_tag.get(),
            expected: clip_type_tag().get(),
        });
    }
    let clip = AnimationClip::from_cooked_payload(payload)
        .map_err(|e| AssetLoadError::Invalid(payload_error(payload, &e)))?;
    Ok((header, clip))
}

/// The text for a payload that does not decode. One that names a version above
/// [`crate::clip::CLIP_FORMAT_VERSION`] (and does not decode as a version 1 clip either) was written by a newer
/// engine: the payload decoder reports that only as a postcard "bad encoding" error, so say what is wrong.
fn payload_error(payload: &[u8], err: &postcard::Error) -> String {
    if let [b'F', b'C', b'L', b'P', a, b, c, d, ..] = payload {
        let claimed = u32::from_le_bytes([*a, *b, *c, *d]);
        if claimed > crate::clip::CLIP_FORMAT_VERSION {
            return format!(
                "cooked clip version {claimed} is newer than this engine reads ({}); upgrade the engine",
                crate::clip::CLIP_FORMAT_VERSION
            );
        }
    }
    err.to_string()
}

/// The cooked-clip payload version of a cooked file: `Ok(None)` when its type tag is not a clip's,
/// an error when it is not a cooked file or the clip does not decode.
///
/// # Errors
///
/// Returns the failure as text (it feeds `regia upgrade-project`).
pub fn cooked_clip_file_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let (header, payload) = decode_cooked_file(bytes).map_err(|e| e.to_string())?;
    if header.type_tag != clip_type_tag() {
        return Ok(None);
    }
    let (version, _) =
        AnimationClip::decode_cooked_payload(payload).map_err(|e| payload_error(payload, &e))?;
    Ok(Some(version))
}

/// Rewrites a cooked clip file at the current payload version (`regia upgrade-project`). A
/// version 1 clip gains no node bindings, and a version 1 or 2 clip is written as a lossless
/// version 3 payload (the migrator never compresses: re-cook the source for that, ADR-7227), so
/// it plays exactly as before. A compressed version 3 clip is rewritten from its decompressed keys,
/// losslessly.
///
/// # Errors
///
/// Returns the failure as text when the file is not a cooked clip or does not decode.
pub fn migrate_cooked_clip_file(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let (mut header, payload) = decode_cooked_file(bytes).map_err(|e| e.to_string())?;
    if header.type_tag != clip_type_tag() {
        return Err("not a cooked clip".to_string());
    }
    let clip = AnimationClip::from_cooked_payload(payload).map_err(|e| payload_error(payload, &e))?;
    let out = clip.to_cooked_payload().map_err(|e| e.to_string())?;
    header.payload_len = out.len() as u64;
    regia_asset::encode_cooked_file(&header, &out).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::{JointTrack, TrackCurve};
    use crate::pose::translated;
    use regia_asset::{AssetId, COOKED_FORMAT_VERSION, encode_cooked_file, pack_importer_version};
    use regia_core::math::Vec3;

    // Pin the historical nine-field wire shape independently of AnimationClip's new fields.
    // With no morph tracks, the v1 and v2 track representations coincide.
    fn v1_payload_without_morphs(clip: &AnimationClip) -> Vec<u8> {
        assert!(clip.morph_tracks.is_empty());
        assert!(clip.node_animation.is_none());
        postcard::to_allocvec(&(
            &clip.name,
            clip.duration,
            clip.sample_rate,
            &clip.tracks,
            &clip.morph_tracks,
            &clip.notifies,
            &clip.curves,
            &clip.property_tracks,
            &clip.root_motion,
        ))
        .expect("serialize v1")
    }

    #[test]
    fn skeleton_cook_round_trip() {
        let skeleton = Skeleton::from_local_bind(
            vec!["root".into(), "child".into()],
            vec![None, Some(0)],
            vec![translated(Vec3::ZERO), translated(Vec3::new(0.0, 1.0, 0.0))],
        )
        .expect("valid skeleton");
        let payload = postcard::to_allocvec(&skeleton).expect("serialize");
        let id = AssetId::new_random();
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: skeleton_type_tag(),
            asset_id: id,
            importer_version: pack_importer_version(1, 1, 0),
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        let file = encode_cooked_file(&header, &payload).expect("encode");
        let (hdr, loaded) = load_cooked_skeleton(&file).expect("load");
        assert_eq!(hdr.asset_id, id);
        assert_eq!(loaded.joint_names, skeleton.joint_names);
        assert_eq!(loaded.parents, skeleton.parents);
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn a_cooked_skeleton_that_fails_validate_does_not_load() {
        let good = Skeleton::from_local_bind(
            vec!["a".into(), "b".into()],
            vec![None, Some(0)],
            vec![crate::skeleton::LocalBind::default(); 2],
        )
        .expect("valid skeleton");
        let cycle = Skeleton {
            parents: vec![Some(1), Some(0)],
            ..good.clone()
        };
        let past_the_end = Skeleton {
            parents: vec![Some(5), Some(0)],
            ..good.clone()
        };
        let mut short_bind = good;
        short_bind.bind_pose.truncate(1);
        for skeleton in [cycle, past_the_end, short_bind] {
            let payload = postcard::to_allocvec(&skeleton).expect("serialize");
            let header = CookedHeader {
                format_version: COOKED_FORMAT_VERSION,
                type_tag: skeleton_type_tag(),
                asset_id: AssetId::new_random(),
                importer_version: pack_importer_version(1, 1, 0),
                dependencies: vec![],
                payload_len: payload.len() as u64,
            };
            let file = encode_cooked_file(&header, &payload).expect("encode");
            assert!(matches!(
                load_cooked_skeleton(&file),
                Err(AssetLoadError::Invalid(_))
            ));
        }
    }

    #[test]
    fn clip_cook_round_trip() {
        let mut clip = AnimationClip::empty("test", 1.0);
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Animated {
                times: vec![0.0, 1.0],
                values: vec![Vec3::ZERO, Vec3::new(1.0, 0.0, 0.0)],
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Default::default()),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let payload = clip.to_cooked_payload().expect("serialize");
        let id = AssetId::new_random();
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: clip_type_tag(),
            asset_id: id,
            importer_version: pack_importer_version(1, 1, 0),
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        let file = encode_cooked_file(&header, &payload).expect("encode");
        let (hdr, loaded) = load_cooked_clip(&file).expect("load");
        assert_eq!(hdr.asset_id, id);
        assert_eq!(loaded.name, "test");
        assert!((loaded.duration - 1.0).abs() < 1e-5);
        assert_eq!(loaded.tracks.len(), 1);
    }

    #[test]
    fn rejects_wrong_type_tag() {
        let clip = AnimationClip::empty("x", 1.0);
        let payload = postcard::to_allocvec(&clip).expect("serialize");
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: skeleton_type_tag(), // wrong tag for a clip
            asset_id: AssetId::new_random(),
            importer_version: pack_importer_version(1, 1, 0),
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        let file = encode_cooked_file(&header, &payload).expect("encode");
        let result = load_cooked_clip(&file);
        assert!(result.is_err());
    }

    #[test]
    fn v1_clip_file_reports_version_and_migrates_to_v2() {
        // A version 1 payload is the bare postcard clip (no morph tracks here, so the v1 and v2
        // bodies coincide; the loader reads it through the v1 path).
        let clip = AnimationClip::empty("old", 1.0);
        let payload = v1_payload_without_morphs(&clip);
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: clip_type_tag(),
            asset_id: AssetId::new_random(),
            importer_version: pack_importer_version(1, 1, 0),
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        let file = encode_cooked_file(&header, &payload).expect("encode");
        assert_eq!(cooked_clip_file_version(&file).expect("probe"), Some(1));
        let migrated = migrate_cooked_clip_file(&file).expect("migrate");
        assert_eq!(
            cooked_clip_file_version(&migrated).expect("probe"),
            Some(crate::clip::CLIP_FORMAT_VERSION)
        );
        let (_, loaded) = load_cooked_clip(&migrated).expect("load");
        assert_eq!(loaded, clip);
        let skeleton_file = {
            let mut h = header;
            h.type_tag = skeleton_type_tag();
            encode_cooked_file(&h, &payload).expect("encode")
        };
        assert_eq!(cooked_clip_file_version(&skeleton_file).expect("probe"), None);
    }

    /// `regia upgrade-project` on a version 1 clip whose 70-byte name begins with `CLP`: its
    /// payload starts with the version 2 magic (grok27 diff-core #3), and it still reports
    /// version 1 and migrates instead of failing the file.
    #[test]
    fn a_v1_clip_named_like_the_magic_migrates() {
        let name = format!("CLP{}", "y".repeat(67));
        let clip = AnimationClip::empty(name.as_str(), 1.0);
        let payload = v1_payload_without_morphs(&clip);
        assert_eq!(&payload[..4], b"FCLP", "the collision under test");
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: clip_type_tag(),
            asset_id: AssetId::new_random(),
            importer_version: pack_importer_version(1, 1, 0),
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        let file = encode_cooked_file(&header, &payload).expect("encode");
        assert_eq!(cooked_clip_file_version(&file).expect("probe"), Some(1));
        let migrated = migrate_cooked_clip_file(&file).expect("migrate");
        assert_eq!(
            cooked_clip_file_version(&migrated).expect("probe"),
            Some(crate::clip::CLIP_FORMAT_VERSION)
        );
        let (_, loaded) = load_cooked_clip(&migrated).expect("load");
        assert_eq!(loaded, clip);
        assert_eq!(loaded.name, name);
    }

    #[test]
    fn a_clip_from_a_newer_engine_is_refused_naming_its_version() {
        let mut payload = b"FCLP".to_vec();
        payload.extend_from_slice(&(crate::clip::CLIP_FORMAT_VERSION + 1).to_le_bytes());
        payload.extend_from_slice(&[0, 1, 2, 3]);
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: clip_type_tag(),
            asset_id: AssetId::new_random(),
            importer_version: pack_importer_version(1, 1, 0),
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        let file = encode_cooked_file(&header, &payload).expect("encode");
        for error in [
            cooked_clip_file_version(&file).expect_err("a newer clip is refused"),
            migrate_cooked_clip_file(&file).expect_err("a newer clip is refused"),
        ] {
            assert!(error.contains("newer than this engine"), "{error}");
            assert!(
                error.contains(&(crate::clip::CLIP_FORMAT_VERSION + 1).to_string()),
                "{error}"
            );
        }
    }
}
