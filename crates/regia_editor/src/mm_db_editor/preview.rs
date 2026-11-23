// SPDX-License-Identifier: MPL-2.0
//! The motion-database editor's pose preview (part B, w11-motiondb-editor-b): scrubbing an entry's
//! timeline poses the preview skeleton and shows the feature vector of the frame under the cursor.
//!
//! The frame list and the feature vectors come from the runtime builder itself: the entry is built
//! alone (`MotionDatabaseSource::build` on a copy of the document that holds only that entry, with
//! its tags, spans and trims), and a frame's vector is that database's `raw_features`. Raw features
//! depend only on the entry's own clip (sampling, trajectory and history samples, contact labels
//! over the clip's range), never on the other entries, so they equal the full database's rows for
//! the same frames; normalization (which mixes every clip) is what the full rebuild adds. The
//! frame's tags, landing bias and flags are the runtime's bake (`MotionDatabase::frame_meta`). The
//! pose is the clip sampled at the frame's time on the skeleton's bind pose, drawn with the
//! animation editor's preview widget (`crate::anim::preview::preview_widget`, lines without a GPU
//! device).
//!
//! Loaded skeletons and clips are cached by asset path and built entries by everything their
//! frames depend on, so scrubbing never reloads; an edit of entry `i` rebuilds entry `i` only.

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_animation::clip::AnimationClip;
use regia_animation::motion_matching::{
    ClipRange, EntrySource, FLAG_PASS_THROUGH, FLAG_UNINDEXED, FeatureGroup, FrameMeta, MotionDatabase,
    MotionDatabaseSource, SchemaSource,
};
use regia_animation::pose::Pose;
use regia_animation::skeleton::Skeleton;
use regia_asset_pipeline::motiondb_cook::MotionDbInputs;
use regia_core::math::Mat4;
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use super::MmDbEditor;
use super::model::{RebuildError, error_code};

/// Everything one entry's preview frames depend on.
#[derive(Clone, Debug, PartialEq)]
pub struct PreviewKey {
    skeleton: String,
    schema: SchemaSource,
    tags: Vec<String>,
    mirror_pairs: Vec<(String, String)>,
    entry: EntrySource,
}

impl PreviewKey {
    /// The key of `doc`'s entry `entry`; `None` when there is no such entry.
    #[must_use]
    pub fn of(doc: &MotionDatabaseSource, entry: usize) -> Option<Self> {
        Some(Self {
            skeleton: doc.skeleton.clone(),
            schema: doc.schema.clone(),
            tags: doc.tags.clone(),
            mirror_pairs: doc.mirror_pairs.clone(),
            entry: doc.entries.get(entry)?.clone(),
        })
    }
}

/// One entry built alone by the runtime builder.
#[derive(Clone)]
pub struct EntryPreview {
    /// The skeleton.
    pub skeleton: Arc<Skeleton>,
    /// The entry's clip.
    pub clip: Arc<AnimationClip>,
    /// The single-entry database (one range, two when the entry is mirrored).
    pub database: Arc<MotionDatabase>,
    /// Why the tags, spans or trims were left out of the build (they did not build; the
    /// validator's issue list says why), so `meta` is absent.
    pub note: Option<String>,
}

impl std::fmt::Debug for EntryPreview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntryPreview")
            .field("clip", &self.clip.name)
            .field("frames", &self.database.len())
            .field("note", &self.note)
            .finish()
    }
}

/// A copy of `doc` holding only entry `entry` (with its tags, spans and trims when `annotated`,
/// without them otherwise).
#[must_use]
pub fn single_entry_source(
    doc: &MotionDatabaseSource,
    entry: usize,
    annotated: bool,
) -> Option<MotionDatabaseSource> {
    let mut one = doc.entries.get(entry)?.clone();
    if !annotated {
        one.tags.clear();
        one.spans.clear();
        one.trim = (0.0, 0.0);
    }
    let mut source = doc.clone();
    source.entries = vec![one];
    Some(source)
}

impl EntryPreview {
    /// The range of the entry's frames (`mirrored`: the mirrored copy).
    #[must_use]
    pub fn range(&self, mirrored: bool) -> Option<&ClipRange> {
        self.database.clips().iter().find(|r| r.mirrored == mirrored)
    }

    /// Frames of the entry (each copy has as many).
    #[must_use]
    pub fn frames(&self) -> usize {
        self.range(false).map_or(0, |r| r.frames)
    }

    /// What the preview shows for frame `frame` of the entry (`mirrored`: of its mirrored copy);
    /// `None` past the end or without a mirrored copy.
    #[must_use]
    pub fn frame(&self, entry: usize, frame: usize, mirrored: bool) -> Option<FrameView> {
        let range = self.range(mirrored)?;
        if frame >= range.frames {
            return None;
        }
        let row = range.start + frame;
        let time = self.database.frame(row)?.time;
        let features = self.database.raw_features(row)?.to_vec();
        let schema = self.database.schema();
        let groups = FeatureGroup::ALL
            .iter()
            .copied()
            .filter(|&g| !schema.group_range(g).is_empty())
            .map(|g| {
                let r = schema.group_range(g);
                GroupSlice {
                    group: g.name(),
                    start: r.start,
                    values: features.get(r).map(<[f32]>::to_vec).unwrap_or_default(),
                }
            })
            .collect();
        let meta = if self.note.is_none() {
            self.database.frame_meta(row)
        } else {
            None
        };
        let tag_names = meta
            .map(|m| {
                self.database
                    .tag_table()
                    .names_of(m.tags)
                    .into_iter()
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let mut pose = self.skeleton.bind_pose_pose();
        self.clip.sample_pose(time, &mut pose);
        Some(FrameView {
            entry,
            frame,
            frames: range.frames,
            time,
            mirrored,
            features,
            groups,
            meta,
            tag_names,
            pose,
        })
    }
}

/// One feature group's slice of a frame's vector.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupSlice {
    /// Group name.
    pub group: &'static str,
    /// First dimension.
    pub start: usize,
    /// The raw values.
    pub values: Vec<f32>,
}

/// The frame under the timeline cursor.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameView {
    /// Source entry.
    pub entry: usize,
    /// Frame within the entry.
    pub frame: usize,
    /// The entry's frame count.
    pub frames: usize,
    /// Clip time, seconds.
    pub time: f32,
    /// A frame of the mirrored copy (its features are mirrored; the pose is drawn unmirrored).
    pub mirrored: bool,
    /// The raw feature vector (the runtime builder's row for this frame).
    pub features: Vec<f32>,
    /// The vector split by feature group.
    pub groups: Vec<GroupSlice>,
    /// The runtime's baked tags, landing bias and flags (absent when the spans did not build).
    pub meta: Option<FrameMeta>,
    /// Names of `meta.tags`.
    pub tag_names: Vec<String>,
    /// The sampled local pose.
    pub pose: Pose,
}

impl FrameView {
    /// The agent form (`editor.mm_db_scrub`).
    #[must_use]
    pub fn json(&self) -> Value {
        json!({
            "entry": self.entry,
            "frame": self.frame,
            "frames": self.frames,
            "time": self.time,
            "mirrored": self.mirrored,
            "features": self.features,
            "groups": self.groups.iter().map(|g| json!({
                "group": g.group, "start": g.start, "values": g.values,
            })).collect::<Vec<_>>(),
            "meta": self.meta.map(|m| json!({
                "tags": self.tag_names,
                "bias": m.bias,
                "pass_through": m.flags & FLAG_PASS_THROUGH != 0,
                "unindexed": m.flags & FLAG_UNINDEXED != 0,
                "target": m.is_target(),
            })),
            "joints": self.pose.len(),
        })
    }
}

/// Builds entry `entry` of `doc` alone with skeleton `skeleton` and clip `clip`: with its tags,
/// spans and trims, or without them (and a note) when those do not build.
///
/// # Errors
///
/// `build_failed` (the message starts with the runtime error's code) when the entry does not
/// build even without its tags, spans and trims; `bad_index` without such an entry.
pub fn build_entry(
    doc: &MotionDatabaseSource,
    entry: usize,
    skeleton: Arc<Skeleton>,
    clip: Arc<AnimationClip>,
) -> Result<EntryPreview, RebuildError> {
    let _span = tracing::info_span!(target: "regia_editor", "mm_db_preview_build").entered();
    let no_entry = || RebuildError {
        code: "bad_index",
        message: format!("there is no entry {entry}"),
    };
    let path = doc.entries.get(entry).ok_or_else(no_entry)?.clip.clone();
    let lookup = |p: &str| (p == path).then_some(clip.as_ref());
    let full = single_entry_source(doc, entry, true).ok_or_else(no_entry)?;
    let (database, note) = match full.build(&skeleton, lookup) {
        Ok(db) => (db, None),
        Err(first) => {
            let bare = single_entry_source(doc, entry, false).ok_or_else(no_entry)?;
            let db = bare.build(&skeleton, lookup).map_err(|e| RebuildError {
                code: "build_failed",
                message: format!("{}: {e}", error_code(&e)),
            })?;
            (
                db,
                Some(format!(
                    "tags, spans and trims left out: {}: {first}",
                    error_code(&first)
                )),
            )
        }
    };
    Ok(EntryPreview {
        skeleton,
        clip,
        database: Arc::new(database),
        note,
    })
}

/// The preview caches: loaded assets by path, built entries by [`PreviewKey`].
#[derive(Resource, Default)]
pub struct MmDbPreview {
    session: u64,
    skeletons: BTreeMap<String, Result<Arc<Skeleton>, String>>,
    clips: BTreeMap<String, Result<Arc<AnimationClip>, String>>,
    entries: BTreeMap<usize, (PreviewKey, Result<EntryPreview, RebuildError>)>,
    builds: u64,
}

impl MmDbPreview {
    /// Entry builds since the document was opened (a cache hit builds nothing).
    #[must_use]
    pub fn builds(&self) -> u64 {
        self.builds
    }
}

/// The preview of entry `entry` of the open document, built (or taken from the cache).
///
/// # Errors
///
/// `no_database_open`, `bad_index`, `no_project` (no inputs to load from), `unknown_skeleton`,
/// `unknown_clip`, `build_failed`.
pub fn entry_preview(world: &mut World, entry: usize) -> Result<EntryPreview, RebuildError> {
    let (key, doc, session) = {
        let editor = world.get_resource_or_insert_with(MmDbEditor::default);
        let doc = editor.doc.clone().ok_or_else(|| RebuildError {
            code: "no_database_open",
            message: "no motion database is open".into(),
        })?;
        let key = PreviewKey::of(&doc, entry).ok_or_else(|| RebuildError {
            code: "bad_index",
            message: format!("there is no entry {entry} (there are {})", doc.entries.len()),
        })?;
        (key, doc, editor.session())
    };
    {
        let mut cache = world.get_resource_or_insert_with(MmDbPreview::default);
        if cache.session != session {
            *cache = MmDbPreview {
                session,
                ..MmDbPreview::default()
            };
        }
        if let Some((cached, result)) = cache.entries.get(&entry)
            && *cached == key
        {
            return result.clone();
        }
    }
    let result = load_and_build(world, &doc, entry);
    let mut cache = world.get_resource_or_insert_with(MmDbPreview::default);
    cache.builds += 1;
    cache.entries.insert(entry, (key, result.clone()));
    result
}

/// The rebuild's inputs (memory inputs, or the project's cooked cache), made once per load.
fn ensure_inputs<'a>(
    world: &World,
    inputs: &'a mut Option<Box<dyn MotionDbInputs + Send>>,
) -> Result<&'a mut Box<dyn MotionDbInputs + Send>, RebuildError> {
    if inputs.is_none() {
        *inputs = Some(super::rebuild_inputs(world).map_err(|e| RebuildError {
            code: e.code,
            message: e.message,
        })?);
    }
    inputs.as_mut().ok_or_else(|| RebuildError {
        code: "no_project",
        message: "no inputs".into(),
    })
}

fn load_and_build(
    world: &mut World,
    doc: &MotionDatabaseSource,
    entry: usize,
) -> Result<EntryPreview, RebuildError> {
    let clip_path = doc.entries[entry].clip.clone();
    let cached_skeleton = world
        .get_resource::<MmDbPreview>()
        .and_then(|c| c.skeletons.get(&doc.skeleton).cloned());
    let cached_clip = world
        .get_resource::<MmDbPreview>()
        .and_then(|c| c.clips.get(&clip_path).cloned());
    let mut inputs: Option<Box<dyn MotionDbInputs + Send>> = None;
    let skeleton = match cached_skeleton {
        Some(s) => s,
        None => {
            let loaded = ensure_inputs(world, &mut inputs)?
                .skeleton(&doc.skeleton)
                .map(|(_, s)| Arc::new(s));
            world
                .get_resource_or_insert_with(MmDbPreview::default)
                .skeletons
                .insert(doc.skeleton.clone(), loaded.clone());
            loaded
        }
    }
    .map_err(|e| RebuildError {
        code: "unknown_skeleton",
        message: e,
    })?;
    let clip = match cached_clip {
        Some(c) => c,
        None => {
            let loaded = ensure_inputs(world, &mut inputs)?
                .clip(&clip_path)
                .and_then(|(_, c)| {
                    regia_asset_pipeline::motiondb_cook::scan_clip_finite(&c).map_err(|e| e.message)?;
                    Ok(Arc::new(c))
                });
            world
                .get_resource_or_insert_with(MmDbPreview::default)
                .clips
                .insert(clip_path.clone(), loaded.clone());
            loaded
        }
    }
    .map_err(|e| RebuildError {
        code: "unknown_clip",
        message: format!("entry {entry}: {e}"),
    })?;
    build_entry(doc, entry, skeleton, clip)
}

/// The frame view of the open document's entry `entry`, frame `frame` (`mirrored`: of its
/// mirrored copy).
///
/// # Errors
///
/// The errors of [`entry_preview`], and `bad_frame` past the entry's last frame or for a mirrored
/// frame of an entry that is not mirrored.
pub fn frame_view(
    world: &mut World,
    entry: usize,
    frame: usize,
    mirrored: bool,
) -> Result<FrameView, RebuildError> {
    let preview = entry_preview(world, entry)?;
    preview.frame(entry, frame, mirrored).ok_or_else(|| RebuildError {
        code: "bad_frame",
        message: if mirrored && preview.range(true).is_none() {
            format!("entry {entry} is not mirrored")
        } else {
            format!(
                "frame {frame} is past the end of entry {entry} ({} frames)",
                preview.frames()
            )
        },
    })
}

/// Object-space joint matrices of `pose` on `skeleton` (bind pose when the sizes differ).
#[must_use]
pub fn globals(skeleton: &Skeleton, pose: &Pose) -> Vec<Mat4> {
    if pose.len() == skeleton.len() {
        skeleton.globals(pose, None)
    } else {
        skeleton.globals(&skeleton.bind_pose_pose(), None)
    }
}
