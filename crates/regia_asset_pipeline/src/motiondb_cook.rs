// SPDX-License-Identifier: MPL-2.0
//! Cook `.motiondb.ron` (`docs/specs/52-motion-matching.md` § 4, ADR-2409): resolve the skeleton
//! and clip dependencies, refuse clips with non-finite values, build the feature set and the k-d
//! index once at cook time and store the cooked payload
//! (`regia_animation::motion_matching::MotionDatabase::to_cooked_payload`, `motiondb-cooked` v3, ADR-2858, ADR-7817)
//! in a `.fckd` of type `regia_animation::MotionDatabase`.
//!
//! The header's dependency list has a fixed order the runtime loader relies on
//! (`regia_animation::motion_matching::load_motion_database_file`): the skeleton first, then one
//! clip per source entry, in entry order (a clip two entries share is listed twice).
//!
//! In a project cook ([`ProjectInputs`]) the skeleton and clip paths name glTF sub-assets,
//! `<file>#skeleton/<name>` and `<file>#clip/<name>` (the paths a glTF cook writes into the
//! manifest), project relative with or without `assets/`. They resolve through the glTF's
//! `.meta` id to the cooked files the glTF cook left in the cache, so `cook_project` cooks motion
//! databases after every other source. [`MemoryInputs`] serves tests, samples and tools that make
//! their clips in code.
//!
//! The non-finite scan ([`first_non_finite`], spec 85 § remainder) also runs on every clip a glTF
//! cook writes: a NaN or infinite key fails the cook with `clip_non_finite` instead of shipping
//! a clip that poisons every pose blended with it.

// Source: the cook and payload layout are this engine's own (spec 52 § 4). Original code.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_animation::clip::{AnimationClip, TrackCurve};
use regia_animation::cooked::{load_cooked_clip, load_cooked_skeleton};
use regia_animation::motion_matching::procedural::ProceduralLocomotion;
use regia_animation::motion_matching::{
    MOTION_DATABASE_TYPE_PATH, MotionDatabase, MotionDatabaseSource, MotionMatchError,
};
use regia_animation::skeleton::Skeleton;
use regia_asset::{
    AssetId, COOKED_FORMAT_VERSION, CookedHeader, MetaSidecar, cooked_file_name, encode_cooked_file,
    meta_path_for,
};

/// The importer name a `.motiondb.ron` cooks under (its `.meta`, the cook ledger).
pub const MOTIONDB_IMPORTER: &str = "motiondb_ron";

/// Code of a clip with a NaN or infinite value.
pub const CLIP_NON_FINITE: &str = "clip_non_finite";

/// Why a motion database (or a clip) did not cook. The message names the entry or the clip.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct MotionDbCookError {
    /// `motiondb_parse`, `motiondb_version`, `motiondb_unknown_skeleton`, `motiondb_unknown_clip`,
    /// `motiondb_clip_skeleton`, `clip_non_finite`, `motiondb_build` or `motiondb_write`.
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

impl MotionDbCookError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Where a cook finds the skeleton and the clips a source names.
pub trait MotionDbInputs {
    /// The skeleton at `path` and the asset id the cooked header lists for it.
    ///
    /// # Errors
    ///
    /// Why it cannot be read (becomes `motiondb_unknown_skeleton`).
    fn skeleton(&mut self, path: &str) -> Result<(AssetId, Skeleton), String>;

    /// The clip at `path` and its asset id.
    ///
    /// # Errors
    ///
    /// Why it cannot be read (becomes `motiondb_unknown_clip`).
    fn clip(&mut self, path: &str) -> Result<(AssetId, AnimationClip), String>;
}

/// Skeletons and clips held in memory, by path; their ids are
/// `AssetId::sub_asset(AssetId::default(), path)` (stable across runs).
#[derive(Clone, Debug, Default)]
pub struct MemoryInputs {
    skeletons: BTreeMap<String, Skeleton>,
    clips: BTreeMap<String, AnimationClip>,
}

impl MemoryInputs {
    /// No content.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a skeleton at `path`.
    #[must_use]
    pub fn with_skeleton(mut self, path: impl Into<String>, skeleton: Skeleton) -> Self {
        self.skeletons.insert(path.into(), skeleton);
        self
    }

    /// Add a clip at `path`.
    #[must_use]
    pub fn with_clip(mut self, path: impl Into<String>, clip: AnimationClip) -> Self {
        self.clips.insert(path.into(), clip);
        self
    }

    /// The procedural locomotion set's rig and clips at the paths its source names.
    pub fn procedural(set: &ProceduralLocomotion) -> Self {
        let mut inputs = Self::new().with_skeleton(set.source.skeleton.clone(), set.skeleton.clone());
        for (path, clip) in &set.clips {
            inputs = inputs.with_clip(path.clone(), clip.clone());
        }
        inputs
    }

    /// The id a path gets.
    pub fn id_of(path: &str) -> AssetId {
        AssetId::sub_asset(AssetId::default(), path)
    }
}

impl MotionDbInputs for MemoryInputs {
    fn skeleton(&mut self, path: &str) -> Result<(AssetId, Skeleton), String> {
        let skeleton = self
            .skeletons
            .get(path)
            .ok_or_else(|| format!("no skeleton {path:?}"))?;
        Ok((Self::id_of(path), skeleton.clone()))
    }

    fn clip(&mut self, path: &str) -> Result<(AssetId, AnimationClip), String> {
        let clip = self.clips.get(path).ok_or_else(|| format!("no clip {path:?}"))?;
        Ok((Self::id_of(path), clip.clone()))
    }
}

/// glTF sub-assets of a project, read from the cooked cache the glTF cooks wrote.
#[derive(Clone, Debug)]
pub struct ProjectInputs {
    project_root: PathBuf,
    cache_dir: PathBuf,
}

impl ProjectInputs {
    /// Inputs of the project at `project_root` whose cooked cache is `cache_dir`.
    pub fn new(project_root: &Path, cache_dir: &Path) -> Self {
        Self {
            project_root: project_root.to_path_buf(),
            cache_dir: cache_dir.to_path_buf(),
        }
    }

    /// The id and cooked bytes of `<file>#<sub>`.
    fn read(&self, path: &str) -> Result<(AssetId, Vec<u8>), String> {
        let (file, sub) = path.split_once('#').ok_or_else(|| {
            format!("{path:?} is not a sub-asset path (`<file>#skeleton/<name>` or `<file>#clip/<name>`)")
        })?;
        let rel = file.trim_start_matches("./").replace('\\', "/");
        let candidates = [rel.clone(), format!("assets/{rel}")];
        let source = candidates
            .iter()
            .filter_map(|c| regia_core::project_path::resolve_write(&self.project_root, c).ok())
            .find(|p| p.is_file())
            .ok_or_else(|| format!("{file:?} is not a file of the project"))?;
        let meta_bytes = std::fs::read(meta_path_for(&source))
            .map_err(|e| format!("{file:?} has no readable .meta (cook it first): {e}"))?;
        let meta = MetaSidecar::from_ron(&meta_bytes).map_err(|e| format!("{file:?}.meta: {e}"))?;
        let id = AssetId::sub_asset(meta.id, sub);
        let bytes = std::fs::read(self.cache_dir.join(cooked_file_name(id)))
            .map_err(|e| format!("{path:?} is not in the cooked cache (does {file:?} have it?): {e}"))?;
        Ok((id, bytes))
    }
}

impl MotionDbInputs for ProjectInputs {
    fn skeleton(&mut self, path: &str) -> Result<(AssetId, Skeleton), String> {
        let (id, bytes) = self.read(path)?;
        let (_, skeleton) = load_cooked_skeleton(&bytes).map_err(|e| format!("{path:?}: {e}"))?;
        Ok((id, skeleton))
    }

    fn clip(&mut self, path: &str) -> Result<(AssetId, AnimationClip), String> {
        let (id, bytes) = self.read(path)?;
        let (_, clip) = load_cooked_clip(&bytes).map_err(|e| format!("{path:?}: {e}"))?;
        Ok((id, clip))
    }
}

/// A cooked motion database.
#[derive(Clone, Debug)]
pub struct CookedMotionDb {
    /// The database as built (for reports and tests).
    pub database: MotionDatabase,
    /// The cooked payload (`motiondb-cooked` v3, with its trailing content hash).
    pub payload: Vec<u8>,
    /// The header's dependencies: the skeleton, then one clip per entry.
    pub dependencies: Vec<AssetId>,
    /// Build warnings (`missing_root_motion` and the like), one line each.
    pub warnings: Vec<String>,
}

fn curve_non_finite<T: Copy>(curve: &TrackCurve<T>, finite: impl Fn(T) -> bool) -> Option<String> {
    match curve {
        TrackCurve::Static(v) => (!finite(*v)).then(|| "static value".to_string()),
        TrackCurve::Animated { times, values, .. } => {
            if let Some(k) = times.iter().position(|t| !t.is_finite()) {
                return Some(format!("key {k} time"));
            }
            values
                .iter()
                .position(|v| !finite(*v))
                .map(|k| format!("key {k} value"))
        }
    }
}

/// The first NaN or infinite value of `clip`, described (`None` when every value is finite):
/// the duration, every joint track's keys (times, translations, rotations, scales), morph
/// weights, named curves, property tracks and notify times.
pub fn first_non_finite(clip: &AnimationClip) -> Option<String> {
    if !clip.duration.is_finite() {
        return Some("duration".into());
    }
    if !clip.sample_rate.is_finite() {
        return Some("sample_rate".into());
    }
    for track in &clip.tracks {
        let j = track.joint;
        if let Some(at) = curve_non_finite(&track.translations, |v| v.is_finite()) {
            return Some(format!("joint {j} translation {at}"));
        }
        if let Some(at) = curve_non_finite(&track.rotations, |q| q.is_finite()) {
            return Some(format!("joint {j} rotation {at}"));
        }
        if let Some(at) = curve_non_finite(&track.scales, |v| v.is_finite()) {
            return Some(format!("joint {j} scale {at}"));
        }
    }
    for track in &clip.morph_tracks {
        if let Some(at) = curve_non_finite(&track.weights, f32::is_finite) {
            return Some(format!("morph target {} weight {at}", track.target));
        }
    }
    for curve in &clip.curves {
        if let Some(at) = curve_non_finite(&curve.curve, f32::is_finite) {
            return Some(format!("curve {:?} {at}", curve.name));
        }
    }
    for track in &clip.property_tracks {
        if let Some(at) = curve_non_finite(&track.curve, f32::is_finite) {
            return Some(format!("property {:?} {at}", track.path));
        }
    }
    clip.notifies
        .iter()
        .position(|n| !n.time.is_finite())
        .map(|i| format!("notify {i} time"))
}

/// [`first_non_finite`] as a cook error: `clip_non_finite` naming the clip and the value.
///
/// # Errors
///
/// [`MotionDbCookError`] with code [`CLIP_NON_FINITE`].
pub fn scan_clip_finite(clip: &AnimationClip) -> Result<(), MotionDbCookError> {
    match first_non_finite(clip) {
        Some(at) => Err(MotionDbCookError::new(
            CLIP_NON_FINITE,
            format!("clip {:?}: {at} is not finite", clip.name),
        )),
        None => Ok(()),
    }
}

/// Cook a `.motiondb.ron` text with `inputs`: parse, resolve the skeleton and every entry's
/// clip, scan the clips, build (features, normalization, tags, spans, the k-d tree) and encode
/// the payload. Deterministic: the same text and inputs give the same bytes.
///
/// # Errors
///
/// [`MotionDbCookError`] with the codes listed on it.
pub fn cook_motiondb(
    text: &str,
    inputs: &mut dyn MotionDbInputs,
) -> Result<CookedMotionDb, MotionDbCookError> {
    let _span = tracing::info_span!(target: "regia_asset_pipeline", "motiondb_cook").entered();
    let source = MotionDatabaseSource::from_ron_str(text).map_err(|e| match e {
        MotionMatchError::UnsupportedFormatVersion { .. } => {
            MotionDbCookError::new("motiondb_version", e.to_string())
        }
        other => MotionDbCookError::new("motiondb_parse", other.to_string()),
    })?;
    cook_motiondb_source(&source, inputs)
}

/// [`cook_motiondb`] on a parsed source.
///
/// # Errors
///
/// As [`cook_motiondb`].
pub fn cook_motiondb_source(
    source: &MotionDatabaseSource,
    inputs: &mut dyn MotionDbInputs,
) -> Result<CookedMotionDb, MotionDbCookError> {
    let (skeleton_id, skeleton) = inputs
        .skeleton(&source.skeleton)
        .map_err(|e| MotionDbCookError::new("motiondb_unknown_skeleton", e))?;
    let mut dependencies = vec![skeleton_id];
    let mut clips: BTreeMap<String, AnimationClip> = BTreeMap::new();
    let mut ids: BTreeMap<String, AssetId> = BTreeMap::new();
    for (index, entry) in source.entries.iter().enumerate() {
        if let Some(id) = ids.get(&entry.clip) {
            dependencies.push(*id);
            continue;
        }
        let (id, clip) = inputs
            .clip(&entry.clip)
            .map_err(|e| MotionDbCookError::new("motiondb_unknown_clip", format!("entry {index}: {e}")))?;
        scan_clip_finite(&clip)
            .map_err(|e| MotionDbCookError::new(e.code, format!("entry {index}: {}", e.message)))?;
        if let Some(track) = clip.tracks.iter().find(|t| t.joint as usize >= skeleton.len()) {
            return Err(MotionDbCookError::new(
                "motiondb_clip_skeleton",
                format!(
                    "entry {index}: clip {:?} animates joint {} but the skeleton has {} joints",
                    clip.name,
                    track.joint,
                    skeleton.len()
                ),
            ));
        }
        dependencies.push(id);
        ids.insert(entry.clip.clone(), id);
        clips.insert(entry.clip.clone(), clip);
    }
    let database = source
        .build(&skeleton, |path| clips.get(path))
        .map_err(|e| MotionDbCookError::new("motiondb_build", e.to_string()))?;
    let payload = database
        .to_cooked_payload()
        .map_err(|e| MotionDbCookError::new("motiondb_write", e.to_string()))?;
    let warnings = database
        .warnings()
        .iter()
        .map(|w| {
            let clip = database.clips().get(w.clip).map_or("?", |c| c.name.as_str());
            format!("{}: clip {clip:?}", w.kind.name())
        })
        .collect();
    Ok(CookedMotionDb {
        database,
        payload,
        dependencies,
        warnings,
    })
}

/// The whole `.fckd` file of a cooked database with id `id` (what a cook writes to the cache).
///
/// # Errors
///
/// `motiondb_write` when the container cannot be encoded.
pub fn encode_motiondb_file(
    id: AssetId,
    importer_version: u32,
    cooked: &CookedMotionDb,
) -> Result<Vec<u8>, MotionDbCookError> {
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: regia_core::id::TypePathHash::of(MOTION_DATABASE_TYPE_PATH),
        asset_id: id,
        importer_version,
        dependencies: cooked.dependencies.clone(),
        payload_len: cooked.payload.len() as u64,
    };
    encode_cooked_file(&header, &cooked.payload)
        .map_err(|e| MotionDbCookError::new("motiondb_write", e.to_string()))
}

/// True for a `.motiondb.ron` path (any case).
pub fn is_motiondb(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.to_ascii_lowercase().ends_with(".motiondb.ron"))
}
