// SPDX-License-Identifier: MPL-2.0
//! Incremental cook without the import (ADR-530).
//!
//! The cache key of a cooked source ([`crate::db::DbEntry::cache_key`]) needs the importer's
//! settings hash, which the importer computes, so `cook_one` used to run the whole import (glTF
//! parse, MikkTSpace, LOD build, block compression, sub-asset writes) for every source and only
//! then find the key unchanged. Every `regia run`, editor open and preview paid a full re-import.
//!
//! This module keys each source on **everything its importer reads** instead, without importing:
//! the source bytes, its `.meta` (id and importer settings), for a `.gltf` the buffers and images
//! it references by URI, for a glTF the project's LOD class table, the platform, the importer
//! version, and the build of the running cooker (engine version plus the executable's size and
//! modification time, so a rebuilt or updated engine re-imports as before). When that key matches
//! the last cook's and every cooked output is still on disk, the source is skipped with no import.
//! Only importers whose inputs are exactly that set take this path: glTF/GLB, the image
//! formats (PNG, JPEG, TGA, BMP, DDS, EXR, HDR, SVG, PSD), the audio formats (WAV, OGG, FLAC, MP3)
//! and channel-pack recipes (`.texpack.ron`, whose source images the key covers); see `skippable`.
//! Every other importer runs as before.
//!
//! **Stamps.** Hashing a source still means reading it. A file whose size and modification time
//! match the last hash reuses that hash without a read (the approach `make` and Ninja take for
//! their up-to-date checks, and git's index for stat data, `Documentation/technical/racy-git.txt`).
//! A file modified within [`RACY_MARGIN_NS`] of the moment it was hashed is "racily clean" (an
//! edit in the same timestamp tick would keep the stamp), so it is hashed again. A file rewritten
//! with the same size and a restored modification time (`touch -r`, `cp -p` of other content)
//! would be missed: `regia cook --clean` cooks from scratch.
//!
//! The stamps and keys live in `.regia/assetdb/cook-inputs-<platform>.bin`. It is a cache, not a
//! content format: a missing, unreadable or other-revision file is ignored, which costs one
//! re-hash and one import of each source.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use regia_asset::{ManifestEntry, cooked_file_name, meta_path_for};
use serde::{Deserialize, Serialize};

use crate::db::DbEntry;

/// Layout revision of the inputs file; a file of another revision is ignored.
const INPUTS_REVISION: u32 = 1;

/// A file modified this close to (or after) the moment it was hashed is hashed again next time:
/// an edit within the file system's timestamp granularity (2 s on FAT, 1 s on some network file
/// systems) could leave its stamp unchanged.
pub(crate) const RACY_MARGIN_NS: u64 = 2_000_000_000;

/// The extensions whose importer reads only the inputs [`CookInputs::input_key`] covers.
pub(crate) fn skippable(ext: &str) -> bool {
    matches!(
        ext,
        "gltf"
            | "glb"
            | "png"
            | "jpg"
            | "jpeg"
            | "tga"
            | "bmp"
            | "dds"
            | "exr"
            | "hdr"
            | "svg"
            | "psd"
            | "wav"
            | "ogg"
            | "flac"
            | "mp3"
            // Channel-pack recipes (`cook_one` gives them this ext): the sources are side files.
            | "texpack.ron"
    )
}

/// The files besides the source and its `.meta` that the importer of `ext` reads and the input
/// key follows (with each one's `.meta`): a `.gltf`'s URIs, a `.texpack.ron`'s source images and
/// nested recipes (ADR-3090); none for every other importer.
pub(crate) fn sidecars_for(source: &Path, ext: &str) -> Vec<PathBuf> {
    match ext {
        "gltf" => crate::cook::gltf_sidecars_of(source),
        "texpack.ron" => crate::texture_pack::recipe_inputs(source),
        _ => Vec::new(),
    }
}

/// What a file looked like when it was hashed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    len: u64,
    /// Modification time, ns since the Unix epoch.
    mtime_ns: u64,
    /// When the bytes were read, ns since the Unix epoch.
    hashed_at_ns: u64,
    hash: [u8; 32],
}

/// The inputs of one source's last successful cook.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct InputRecord {
    key: [u8; 32],
    /// The primary output's reflect path.
    type_path: String,
    /// The sub-assets the import wrote (glTF meshes, materials, textures, clips, ...).
    sub_manifest: Vec<ManifestEntry>,
    /// The files a `.gltf` references by URI (absolute), in the order the key hashed them.
    sidecars: Vec<PathBuf>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct InputsFile {
    revision: u32,
    /// By absolute path.
    stamps: BTreeMap<PathBuf, Stamp>,
    /// By project-relative source path (`assets/...`).
    records: BTreeMap<String, InputRecord>,
}

/// Counters of one cook (reported in [`crate::CookReport`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct InputCounters {
    /// Sources skipped without running their importer.
    pub(crate) imports_skipped: usize,
    /// Bytes read to hash sources and their inputs (files whose stamp matched are not read).
    pub(crate) hashed_bytes: u64,
}

/// The stamps and input keys of one project and platform, open for one cook.
pub(crate) struct CookInputs {
    path: PathBuf,
    project_root: PathBuf,
    platform: String,
    file: InputsFile,
    /// Identity of the running cooker.
    build: String,
    pub(crate) counters: InputCounters,
    /// The project's texture policy for this platform (`[cook.textures]`, ADR-2063), or the
    /// error reading it.
    texture_policy: Result<crate::TexturePolicy, String>,
    /// The project's `[cook] texture_quality` (spec 21 § 2, ADR-7217), or the error reading it.
    texture_quality: Result<crate::ProjectTextureQuality, String>,
    /// The project's `[content.diet]` (ADR-8461): `None` without one, `Err` when it does not parse
    /// (the cook fails on it).
    diet: Result<Option<crate::cook_diet::DietRules>, String>,
    /// The project's import settings rules (`config/import_rules.ron`, ADR-3321): `None` when the
    /// file is absent (the common case), `Err` when it does not parse (the cook fails on it).
    import_rules: Result<Option<regia_asset::import_rules::ImportRulesFile>, String>,
    /// The stamp of the read that produced `import_rules` (one stat, taken with that read):
    /// [`Self::import_rules_unchanged`] answers from it while it is trusted and unmoved.
    rules_stamp: crate::import_rules::RulesStamp,
    /// The shared cook cache of this cook, when one is attached (ADR-2612).
    pub(crate) shared: Option<crate::cook_cache::SharedCookCache>,
}

/// Which key `CookInputs::key_with` computes: the local ADR-530 key or the shared ADR-2612 one.
struct KeyKind<'a> {
    tag: &'static [u8],
    revision: u32,
    /// The cooker identity (local) or the engine build id (shared).
    build: &'a str,
    /// Read the `.meta` and side files instead of trusting their stamps.
    fresh: bool,
}

/// One source about to be cooked: what the key needs to know about it.
pub(crate) struct SourceInputs<'a> {
    pub(crate) source: &'a Path,
    pub(crate) rel_path: &'a str,
    pub(crate) project_root: &'a Path,
    pub(crate) platform: &'a str,
    /// Lower-case extension.
    pub(crate) ext: &'a str,
    pub(crate) importer_version: u32,
    /// Extra key text (the Android ASTC policy, a `draft` texture quality).
    pub(crate) revision: &'a str,
}

impl CookInputs {
    /// The inputs file of `project_root` for `platform` (empty when missing or unreadable).
    pub(crate) fn open(project_root: &Path, platform: &str) -> Self {
        let path = inputs_path(project_root, platform);
        let file = std::fs::read(&path)
            .ok()
            .and_then(|bytes| postcard::from_bytes::<InputsFile>(&bytes).ok())
            .filter(|f| f.revision == INPUTS_REVISION)
            .unwrap_or_default();
        let (import_rules, rules_stamp) = crate::import_rules::for_project_stamped(project_root);
        Self {
            path,
            project_root: project_root.to_path_buf(),
            platform: platform.to_string(),
            file,
            build: build_stamp(),
            counters: InputCounters::default(),
            texture_policy: crate::TexturePolicy::for_project(project_root, platform),
            texture_quality: crate::ProjectTextureQuality::for_project(project_root),
            diet: crate::cook_diet::DietRules::for_profile(project_root, None).map_err(|e| e.to_string()),
            import_rules: import_rules.map_err(|e| e.to_string()),
            rules_stamp,
            shared: None,
        }
    }

    /// The texture policy this cook applies (the default when `Regia.toml` holds a bad one; the
    /// cook reports that as an error, [`Self::texture_policy_error`]).
    pub(crate) fn texture_policy(&self) -> crate::TexturePolicy {
        self.texture_policy.as_ref().copied().unwrap_or_default()
    }

    /// Why the project's texture policy could not be read, if it could not.
    pub(crate) fn texture_policy_error(&self) -> Option<&str> {
        self.texture_policy.as_ref().err().map(String::as_str)
    }

    /// The project's texture quality (`final` when `Regia.toml` holds a bad one; the cook reports
    /// that as an error, [`Self::texture_quality_error`]).
    pub(crate) fn texture_quality(&self) -> crate::ProjectTextureQuality {
        self.texture_quality.as_ref().copied().unwrap_or_default()
    }

    /// Why the project's `[cook] texture_quality` could not be read, if it could not.
    pub(crate) fn texture_quality_error(&self) -> Option<&str> {
        self.texture_quality.as_ref().err().map(String::as_str)
    }

    /// The cook diet's edge cap of the textures the source `rel_path` cooks (`None` without a diet,
    /// or with one that does not parse: the cook reports that, [`Self::diet_error`]).
    pub(crate) fn diet_texture_cap(&self, rel_path: &str) -> Option<u32> {
        self.diet
            .as_ref()
            .ok()
            .and_then(Option::as_ref)?
            .cap_for(rel_path)
    }

    /// Why the project's `[content.diet]` could not be read, if it could not.
    pub(crate) fn diet_error(&self) -> Option<&str> {
        self.diet.as_ref().err().map(String::as_str)
    }

    /// Why the project's import rules could not be read, if they could not (the cook fails like a
    /// bad `Regia.toml`, so no source cooks with settings the rules should have set).
    pub(crate) fn import_rules_error(&self) -> Option<&str> {
        self.import_rules.as_ref().err().map(String::as_str)
    }

    /// Whether `config/import_rules.ron` still reads as it did when this cook opened. The keys
    /// hash that snapshot, but an import re-reads the file (`resolve_for_cook`): an output cooked
    /// after an edit must not be recorded (or published) under the snapshot's key. A trusted,
    /// unmoved `(len, mtime)` stamp answers for an untouched file with one stat (rev-corr-assets,
    /// sprint-corr-asset-pipeline-a 3); a moved, racy or missing stamp falls back to comparing
    /// the parsed content, which a same-tick, same-length edit cannot fool (rev-kimi-5 finding 1).
    pub(crate) fn import_rules_unchanged(&self) -> bool {
        if self.rules_stamp.still_matches(&self.project_root) {
            return true;
        }
        crate::import_rules::for_project_cached(&self.project_root).map_err(|e| e.to_string())
            == self.import_rules
    }

    /// The project this cook belongs to.
    pub(crate) fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// The platform tag of this cook.
    pub(crate) fn platform(&self) -> &str {
        &self.platform
    }

    /// Writes the inputs file (temp file + rename). Stamps of files that are gone are dropped.
    pub(crate) fn save(&mut self) -> io::Result<()> {
        self.file.revision = INPUTS_REVISION;
        self.file.stamps.retain(|path, _| path.is_file());
        let bytes = postcard::to_allocvec(&self.file).map_err(io::Error::other)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // A pid + counter temp name: two saves in one process must not share it.
        crate::cook_ledger::write_atomic(&self.path, &bytes)
    }

    /// The blake3 hash of `path`'s bytes, and the bytes when they had to be read: a file whose
    /// size and modification time match its stamp (and was not modified around the time it was
    /// hashed) is not read.
    pub(crate) fn hash_file(&mut self, path: &Path) -> io::Result<([u8; 32], Option<Vec<u8>>)> {
        let key = absolute(path);
        let meta = std::fs::metadata(&key)?;
        let mtime = mtime_ns(&meta);
        if let (Some(mtime), Some(stamp)) = (mtime, self.file.stamps.get(&key))
            && stamp_trusted(stamp, meta.len(), mtime)
        {
            return Ok((stamp.hash, None));
        }
        // Taken before the read: an edit during or after it lands at or after this time.
        let hashed_at_ns = now_ns();
        let bytes = std::fs::read(&key)?;
        self.counters.hashed_bytes += bytes.len() as u64;
        let hash = *blake3::hash(&bytes).as_bytes();
        match mtime {
            // Stamp what was stat'ed before the read; a change in between only makes the next
            // stat disagree with it (a re-hash, never a stale hit).
            Some(mtime_ns) if bytes.len() as u64 == meta.len() => {
                self.file.stamps.insert(
                    key,
                    Stamp {
                        len: meta.len(),
                        mtime_ns,
                        hashed_at_ns,
                        hash,
                    },
                );
            }
            _ => {
                self.file.stamps.remove(&key);
            }
        }
        Ok((hash, Some(bytes)))
    }

    /// [`Self::hash_file`] that always reads `path` (its stamp is replaced): the hash of the
    /// bytes on disk now, never a stamp's. A file rewritten with its size and modification time
    /// kept fools a stamp (the ADR-530 blind spot); that must never key a shared cook cache entry
    /// (ADR-2612).
    pub(crate) fn hash_file_fresh(&mut self, path: &Path) -> io::Result<([u8; 32], Vec<u8>)> {
        self.file.stamps.remove(&absolute(path));
        let (hash, bytes) = self.hash_file(path)?;
        let bytes = match bytes {
            Some(bytes) => bytes,
            None => std::fs::read(path)?,
        };
        Ok((hash, bytes))
    }

    /// Whether a shared cook cache is attached and still in use for this cook (ADR-2612).
    pub(crate) fn shared_active(&self) -> bool {
        self.shared
            .as_ref()
            .is_some_and(crate::cook_cache::SharedCookCache::active)
    }

    /// The key of everything `src`'s importer reads (see the module docs), with `source_hash` the
    /// source's own content hash and `sidecars` the files a `.gltf` references. `None` when the
    /// source has no `.meta` yet (its first cook writes it).
    pub(crate) fn input_key(
        &mut self,
        src: &SourceInputs<'_>,
        source_hash: &[u8; 32],
        sidecars: &[PathBuf],
    ) -> Option<[u8; 32]> {
        let build = std::mem::take(&mut self.build);
        let kind = KeyKind {
            tag: b"regia-cook-inputs",
            revision: INPUTS_REVISION,
            build: &build,
            fresh: false,
        };
        let key = self.key_with(&kind, src, source_hash, sidecars);
        self.build = build;
        key
    }

    /// The shared cook cache key (ADR-2612): [`Self::input_key`]'s fields with the running
    /// executable's stamp replaced by `engine_build_id` (`cook_cache::engine_build_id`), under its
    /// own tag and the entry version. Every field is the same on each machine with the same
    /// project files and engine build. With `fresh` the `.meta` and side files are read and
    /// hashed again instead of trusting their stamps (`hash_file_fresh`): a key that fetches or
    /// publishes a shared entry is always fresh; `source_hash` must be fresh then too.
    pub(crate) fn shared_key(
        &mut self,
        engine_build_id: &str,
        src: &SourceInputs<'_>,
        source_hash: &[u8; 32],
        sidecars: &[PathBuf],
        fresh: bool,
    ) -> Option<[u8; 32]> {
        let kind = KeyKind {
            tag: b"regia-shared-cook",
            revision: crate::cook_cache::ENTRY_VERSION,
            build: engine_build_id,
            fresh,
        };
        self.key_with(&kind, src, source_hash, sidecars)
    }

    /// The key of `kind` over every input of `src` (see the module docs).
    fn key_with(
        &mut self,
        kind: &KeyKind<'_>,
        src: &SourceInputs<'_>,
        source_hash: &[u8; 32],
        sidecars: &[PathBuf],
    ) -> Option<[u8; 32]> {
        let fresh = kind.fresh;
        let hash_of = |inputs: &mut Self, path: &Path| -> io::Result<[u8; 32]> {
            if fresh {
                inputs.hash_file_fresh(path).map(|(hash, _)| hash)
            } else {
                inputs.hash_file(path).map(|(hash, _)| hash)
            }
        };
        let meta_hash = hash_of(self, &meta_path_for(src.source)).ok()?;
        let mut h = blake3::Hasher::new();
        let mut field = |bytes: &[u8]| {
            h.update(&(bytes.len() as u64).to_le_bytes());
            h.update(bytes);
        };
        field(kind.tag);
        field(kind.revision.to_le_bytes().as_slice());
        field(kind.build.as_bytes());
        field(src.platform.as_bytes());
        field(src.ext.as_bytes());
        field(src.importer_version.to_le_bytes().as_slice());
        field(src.revision.as_bytes());
        field(src.rel_path.as_bytes());
        field(source_hash.as_slice());
        field(meta_hash.as_slice());
        // The import rules this source matches (ADR-3321): editing a rule, or adding one that
        // matches, changes the key of exactly the sources it matches — an incremental cook
        // recooks those and nothing else. No rules file or no match hashes nothing, so projects
        // without rules keep every key they had.
        if let Ok(Some(rules)) = &self.import_rules {
            let importer = crate::import_rules::importer_for_ext(src.ext);
            for (_, rule) in rules.matched_rules(src.rel_path, importer) {
                let text = crate::import_rules::rule_key_text(rule);
                field(text.as_bytes());
            }
            // A glTF's referenced images take the rules that match them too
            // (`import_rules::referenced_settings`); a sidecar no rule matches hashes nothing.
            for sidecar in sidecars {
                let Some(rel) = crate::import_rules::project_rel(src.project_root, sidecar) else {
                    continue;
                };
                let ext = sidecar
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                for (_, rule) in rules.matched_rules(&rel, crate::import_rules::importer_for_ext(&ext)) {
                    field(b"sidecar_rule");
                    field(rel.as_bytes());
                    field(crate::import_rules::rule_key_text(rule).as_bytes());
                }
            }
        }
        for path in side_files(src.project_root, src.ext, sidecars) {
            // Project-relative, so the same project cooked through another path keys the same.
            let name = path.strip_prefix(src.project_root).unwrap_or(&path);
            field(name.to_string_lossy().as_bytes());
            match hash_of(self, &path) {
                Ok(hash) => field(hash.as_slice()),
                Err(_) => field(b"<missing>".as_slice()),
            }
        }
        Some(*h.finalize().as_bytes())
    }

    /// The hashes of the files besides the source and its `.meta` that the importer of `ext`
    /// reads (`side_files`); `None` for a missing file.
    pub(crate) fn side_inputs(
        &mut self,
        project_root: &Path,
        ext: &str,
        sidecars: &[PathBuf],
    ) -> Vec<Option<[u8; 32]>> {
        side_files(project_root, ext, sidecars)
            .iter()
            .map(|path| self.hash_file(path).ok().map(|(hash, _)| hash))
            .collect()
    }

    /// Whether the inputs of `rel_path`'s last cook are recorded (its next cook may skip).
    pub(crate) fn has_record(&self, rel_path: &str) -> bool {
        self.file.records.contains_key(rel_path)
    }

    /// Forgets the inputs of `rel_path`: its next cook imports.
    pub(crate) fn forget(&mut self, rel_path: &str) {
        self.file.records.remove(rel_path);
    }

    /// The sidecars recorded for `rel_path` by its last cook (a `.gltf`'s URIs do not change
    /// unless its bytes do, and then the key differs anyway).
    pub(crate) fn recorded_sidecars(&self, rel_path: &str) -> Vec<PathBuf> {
        self.file
            .records
            .get(rel_path)
            .map(|r| r.sidecars.clone())
            .unwrap_or_default()
    }

    /// The outcome of skipping `rel_path` without an import: its recorded key equals `key`, the
    /// database row is the one that cook wrote (`source_hash`, `importer_version`), and the
    /// primary and every sub-asset cooked file are still in `cache_dir`. `None` otherwise.
    pub(crate) fn try_skip(
        &self,
        rel_path: &str,
        key: &[u8; 32],
        source_hash: &[u8; 32],
        importer_version: u32,
        row: Option<&DbEntry>,
        cache_dir: &Path,
    ) -> Option<crate::cook::CookOutcome> {
        let record = self.file.records.get(rel_path)?;
        let row = row?;
        if &record.key != key
            || &row.source_hash != source_hash
            || row.importer_version != importer_version
            || !cache_dir.join(&row.cooked_rel).is_file()
            || !record
                .sub_manifest
                .iter()
                .all(|sub| cache_dir.join(cooked_file_name(sub.id)).is_file())
        {
            return None;
        }
        Some(crate::cook::CookOutcome {
            primary_id: row.asset_id,
            type_path: record.type_path.clone(),
            source_hash: row.source_hash,
            settings_hash: row.settings_hash,
            importer: row.importer.clone(),
            importer_version: row.importer_version,
            cache_key: row.cache_key,
            cooked_rel: row.cooked_rel.clone(),
            skipped: true,
            sub_manifest: record.sub_manifest.clone(),
        })
    }

    /// Records the inputs of a finished cook of `rel_path` (`None` key: forget them).
    pub(crate) fn record(
        &mut self,
        rel_path: &str,
        key: Option<[u8; 32]>,
        outcome: &crate::cook::CookOutcome,
        sidecars: Vec<PathBuf>,
    ) {
        match key {
            Some(key) => {
                self.file.records.insert(
                    rel_path.to_string(),
                    InputRecord {
                        key,
                        type_path: outcome.type_path.clone(),
                        sub_manifest: outcome.sub_manifest.clone(),
                        sidecars,
                    },
                );
            }
            None => self.forget(rel_path),
        }
    }
}

/// The files besides the source and its `.meta` that the importer of `ext` reads: the files a
/// `.gltf` references by URI (`sidecars`) and their `.meta` overrides (a referenced image's
/// texture settings), a `.texpack.ron`'s sources (`sidecars`, without their `.meta`), and for a
/// glTF the project's LOD class table.
fn side_files(project_root: &Path, ext: &str, sidecars: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::with_capacity(sidecars.len() * 2 + 1);
    let gltf = matches!(ext, "gltf" | "glb");
    for sidecar in sidecars {
        files.push(sidecar.clone());
        // A glTF applies a referenced image's `.meta` overrides; a `.texpack.ron` reads only its
        // sources' pixels, so their `.meta` (written when they first cook, maybe after the
        // recipe) is not its input.
        if gltf {
            files.push(meta_path_for(sidecar));
        }
    }
    if matches!(ext, "gltf" | "glb") {
        files.push(project_root.join(crate::lod_classes::LOD_CLASSES_FILE));
    }
    // A sound's gain follows the project's loudness policy (ADR-1855).
    if crate::audio_cook::is_audio_extension(ext) {
        files.push(project_root.join(crate::audio_cook::LOUDNESS_CONFIG_FILE));
        files.push(project_root.join(crate::audio_transcode::AUDIO_COOK_CONFIG_FILE));
    }
    files
}

/// Whether a stamp still describes a file of `len` bytes modified at `mtime_ns`: same size and
/// time, and the modification was at least [`RACY_MARGIN_NS`] before the hash was taken.
fn stamp_trusted(stamp: &Stamp, len: u64, mtime_ns: u64) -> bool {
    stamp.len == len
        && stamp.mtime_ns == mtime_ns
        && mtime_ns.saturating_add(RACY_MARGIN_NS) < stamp.hashed_at_ns
}

/// `.regia/assetdb/cook-inputs-<platform>.bin` (next to the asset database).
pub(crate) fn inputs_path(project_root: &Path, platform: &str) -> PathBuf {
    let db = regia_asset::asset_db_path(project_root);
    let dir = db
        .parent()
        .map_or_else(|| project_root.to_path_buf(), Path::to_path_buf);
    dir.join(format!("cook-inputs-{platform}.bin"))
}

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn mtime_ns(meta: &std::fs::Metadata) -> Option<u64> {
    let since = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(since.as_nanos()).ok()
}

/// Now, ns since the Unix epoch (`0` when the clock is before it).
pub(crate) fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_nanos()).ok())
        .unwrap_or(0)
}

/// Identity of the running cooker: the pipeline's version plus the executable's size and
/// modification time (changes with every rebuilt or updated engine binary).
fn build_stamp() -> String {
    let exe = std::env::current_exe()
        .and_then(|p| p.metadata())
        .map(|m| format!("{}:{}", m.len(), mtime_ns(&m).unwrap_or(0)))
        .unwrap_or_else(|_| "unknown-exe".into());
    format!("{}:{exe}", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The documented import-skip set (module docs, book `startup-assets`) is exactly this list.
    /// Review rc1-rev-editor-k finding 10: PSD and `.texpack.ron` joined the set without the docs
    /// or this list; the docs line up with `skippable` again.
    #[test]
    fn skippable_set_matches_docs() {
        for ext in [
            "gltf",
            "glb",
            "png",
            "jpg",
            "jpeg",
            "tga",
            "bmp",
            "dds",
            "exr",
            "hdr",
            "svg",
            "psd",
            "wav",
            "ogg",
            "flac",
            "mp3",
            "texpack.ron",
        ] {
            assert!(skippable(ext), "{ext}");
        }
        for ext in ["ttf", "wgsl", "ron", "fbx", "tmx"] {
            assert!(!skippable(ext), "{ext}");
        }
    }

    fn set_mtime(path: &Path, ago: Duration) {
        let file = std::fs::OpenOptions::new().write(true).open(path).expect("open");
        file.set_modified(SystemTime::now() - ago).expect("set mtime");
    }

    fn stamp(len: u64, mtime_ns: u64, hashed_at_ns: u64) -> Stamp {
        Stamp {
            len,
            mtime_ns,
            hashed_at_ns,
            hash: [7; 32],
        }
    }

    #[test]
    fn a_stamp_is_trusted_only_for_the_same_file_well_before_its_hash() {
        let hashed = 100 * RACY_MARGIN_NS;
        let old = hashed - 2 * RACY_MARGIN_NS;
        assert!(stamp_trusted(&stamp(10, old, hashed), 10, old));
        // Another size or time is another file.
        assert!(!stamp_trusted(&stamp(10, old, hashed), 11, old));
        assert!(!stamp_trusted(&stamp(10, old, hashed), 10, old + 1));
        // Modified within the margin of (or after) the hash: racily clean, hash again.
        let racy = hashed - RACY_MARGIN_NS;
        assert!(!stamp_trusted(&stamp(10, racy, hashed), 10, racy));
        assert!(!stamp_trusted(&stamp(10, hashed + 1, hashed), 10, hashed + 1));
    }

    /// rev-corr-assets (sprint-corr-asset-pipeline-a 3): `import_rules_unchanged` answers from
    /// a `(len, mtime)` stat stamp — an untouched rules file is "unchanged" (without a read or
    /// parse once its mtime is older than the racy margin), an edited file is not.
    #[test]
    fn import_rules_unchanged_tracks_the_rules_file_stat() {
        let root = regia_test_utils::unique_temp_dir("cook-inputs-rules-stamp");
        std::fs::create_dir_all(&root).expect("dir");
        let rules = root.join(regia_asset::import_rules::IMPORT_RULES_FILE);
        std::fs::create_dir_all(rules.parent().expect("config")).expect("config dir");
        let before = CookInputs::open(&root, "linux");
        assert!(
            before.import_rules_unchanged(),
            "no rules file at open and none now"
        );
        std::fs::write(
            &rules,
            r#"(rules: [(glob: "assets/**", settings: "(compress: Some(false))")])"#,
        )
        .expect("rules");
        assert!(
            !before.import_rules_unchanged(),
            "a cook that opened before the file appeared sees the stamp move"
        );
        let inputs = CookInputs::open(&root, "linux");
        assert!(inputs.import_rules_unchanged(), "untouched since the open");
        std::fs::write(
            &rules,
            r#"(rules: [(glob: "assets/**", settings: "(compress: Some(true))")])"#,
        )
        .expect("edit rules");
        assert!(!inputs.import_rules_unchanged(), "the edit moves the stamp");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A project with `config/import_rules.ron` holding `rules`, and the rules file's path.
    fn rules_project(tag: &str, rules: &str) -> (PathBuf, PathBuf) {
        let root = regia_test_utils::unique_temp_dir(tag);
        let file = root.join(regia_asset::import_rules::IMPORT_RULES_FILE);
        std::fs::create_dir_all(file.parent().expect("config")).expect("config dir");
        std::fs::write(&file, rules).expect("rules");
        (root, file)
    }

    const RULES_64: &str = r#"(rules: [(glob: "assets/**", settings: "(max_size: Some(64))")])"#;
    const RULES_32: &str = r#"(rules: [(glob: "assets/**", settings: "(max_size: Some(32))")])"#;

    /// fixrev5-ldtk-rules-stamp (rev-kimi-5 finding 1): a same-length rules edit inside one
    /// mtime tick of the previous write (coarse FAT/exFAT/HFS+/network mtimes, or a tool writing
    /// right after the cook opened; simulated by putting the first write's mtime back) leaves
    /// the `(len, mtime)` stamp unmoved. The cached parse must not hide it, and the publish
    /// guard must not call the rules unchanged.
    #[test]
    fn a_same_tick_same_length_rules_edit_is_seen() {
        let (root, rules) = rules_project("cook-inputs-rules-racy", RULES_64);
        assert_eq!(RULES_64.len(), RULES_32.len(), "the edit keeps the length");
        let mtime = std::fs::metadata(&rules)
            .expect("meta")
            .modified()
            .expect("mtime");
        let inputs = CookInputs::open(&root, "linux");
        std::fs::write(&rules, RULES_32).expect("edit rules");
        std::fs::File::options()
            .write(true)
            .open(&rules)
            .expect("open rules")
            .set_modified(mtime)
            .expect("same tick");
        let parsed = crate::import_rules::for_project_cached(&root)
            .expect("read")
            .expect("rules");
        assert!(
            parsed.rules[0].settings.contains("32"),
            "stale cached parse: {}",
            parsed.rules[0].settings
        );
        assert!(
            !inputs.import_rules_unchanged(),
            "the guard missed a same-tick rules edit"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// fixrev5-ldtk-rules-stamp (rev-kimi-5 finding 1, second hole): an edit landing between
    /// the parse `CookInputs::open` snapshots and the stamp it keeps must not make the guard
    /// call the rules unchanged while the snapshot holds the old parse.
    #[test]
    fn a_rules_edit_right_after_the_open_read_is_not_unchanged() {
        let (root, rules) = rules_project("cook-inputs-rules-two-stat", RULES_64);
        let edited = rules.clone();
        crate::import_rules::test_seam::on_next_read(move || {
            std::fs::write(
                &edited,
                r#"(rules: [(glob: "assets/**", settings: "(max_size: Some(1024))")])"#,
            )
            .expect("edit rules");
        });
        let inputs = CookInputs::open(&root, "linux");
        let snapshot = inputs.import_rules.clone().expect("parses").expect("rules");
        assert!(
            snapshot.rules[0].settings.contains("64"),
            "the open read the file before the edit"
        );
        assert!(
            !inputs.import_rules_unchanged(),
            "the snapshot holds the pre-edit rules, so the rules changed since the open"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// fixrev5-ldtk-rules-stamp: the guard still answers "unchanged" for an untouched rules
    /// file (old mtime, trusted stamp) and for a racy one whose content is the same (the
    /// content-comparison fallback), and an untouched old file is read once per thread.
    #[test]
    fn an_untouched_rules_file_stays_unchanged_and_is_read_once() {
        let (root, rules) = rules_project("cook-inputs-rules-untouched", RULES_64);
        // Racy (just written): the guard compares content, which matches.
        let racy = CookInputs::open(&root, "linux");
        assert!(
            racy.import_rules_unchanged(),
            "a racy, unedited file is unchanged"
        );
        set_mtime(&rules, Duration::from_secs(60));
        let inputs = CookInputs::open(&root, "linux");
        let reads = crate::import_rules::test_seam::reads();
        for _ in 0..3 {
            assert!(inputs.import_rules_unchanged(), "untouched since the open");
            crate::import_rules::for_project_cached(&root)
                .expect("read")
                .expect("rules");
        }
        assert_eq!(
            crate::import_rules::test_seam::reads(),
            reads,
            "an old, untouched rules file is served from the stamp and the cache"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Audit concurrency-1103 pipeline M1: two saves of one inputs file in one process (the
    /// live cooker's thread and a host cook) shared the temp name `<file>.<pid>.tmp`, so one
    /// truncated the other's half-written temp or found it already renamed away. Every save
    /// succeeds and leaves a file that decodes to one of the two writers' tables.
    #[test]
    fn concurrent_saves_in_one_process_do_not_share_a_temp_file() {
        let root = regia_test_utils::unique_temp_dir("cook-inputs-concurrent");
        let files: Vec<PathBuf> = (0..400)
            .map(|i| {
                let file = root.join(format!("src{i}.png"));
                std::fs::write(&file, format!("bytes {i}")).expect("write");
                file
            })
            .collect();
        let make = |take: usize| {
            let mut inputs = CookInputs::open(&root, "linux");
            for file in files.iter().take(take) {
                inputs.hash_file(file).expect("hash");
            }
            inputs
        };
        let mut failures = Vec::new();
        for round in 0..40 {
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let handles: Vec<_> = [make(400), make(399)]
                .into_iter()
                .map(|mut inputs| {
                    let barrier = barrier.clone();
                    std::thread::spawn(move || {
                        barrier.wait();
                        inputs.save().map_err(|e| e.to_string())
                    })
                })
                .collect();
            for handle in handles {
                if let Err(e) = handle.join().expect("thread") {
                    failures.push(format!("round {round}: {e}"));
                }
            }
            let reopened = CookInputs::open(&root, "linux");
            let n = reopened.file.stamps.len();
            if n != 400 && n != 399 {
                failures.push(format!("round {round}: the saved file holds {n} stamps"));
            }
        }
        assert!(failures.is_empty(), "{failures:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_old_unchanged_file_is_hashed_once_and_an_edit_is_seen() {
        let root = regia_test_utils::unique_temp_dir("cook-inputs-stamps");
        let file = root.join("a.png");
        std::fs::write(&file, b"first").expect("write");
        set_mtime(&file, Duration::from_secs(3600));

        let mut inputs = CookInputs::open(&root, "linux");
        let (first, bytes) = inputs.hash_file(&file).expect("hash");
        assert_eq!(bytes.as_deref(), Some(&b"first"[..]));
        assert_eq!(first, *blake3::hash(b"first").as_bytes());
        assert_eq!(inputs.counters.hashed_bytes, 5);

        // Saved and reopened: the stamp is reused, nothing is read.
        inputs.save().expect("save");
        let mut inputs = CookInputs::open(&root, "linux");
        let (again, bytes) = inputs.hash_file(&file).expect("hash");
        assert_eq!(again, first);
        assert!(bytes.is_none());
        assert_eq!(inputs.counters.hashed_bytes, 0);

        // An edit (new size and time) is read and hashed.
        std::fs::write(&file, b"second!").expect("write");
        set_mtime(&file, Duration::from_secs(1800));
        let (edited, bytes) = inputs.hash_file(&file).expect("hash");
        assert_eq!(edited, *blake3::hash(b"second!").as_bytes());
        assert!(bytes.is_some());
        assert_eq!(inputs.counters.hashed_bytes, 7);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_modified_around_its_hash_is_hashed_again() {
        let root = regia_test_utils::unique_temp_dir("cook-inputs-racy");
        let file = root.join("a.png");
        std::fs::write(&file, b"fresh").expect("write");
        // Just written: its modification time is within the margin of the hash.
        let mut inputs = CookInputs::open(&root, "linux");
        inputs.hash_file(&file).expect("hash");
        let (_, bytes) = inputs.hash_file(&file).expect("hash");
        assert!(bytes.is_some(), "a racily clean file must be read again");
        assert_eq!(inputs.counters.hashed_bytes, 10);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unreadable_or_other_revision_file_starts_empty() {
        let root = regia_test_utils::unique_temp_dir("cook-inputs-revision");
        let path = inputs_path(&root, "linux");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, b"\xff\xff not postcard").expect("write");
        assert!(CookInputs::open(&root, "linux").file.stamps.is_empty());

        let other = InputsFile {
            revision: INPUTS_REVISION + 1,
            stamps: BTreeMap::from([(root.join("x"), stamp(1, 1, 1))]),
            records: BTreeMap::new(),
        };
        std::fs::write(&path, postcard::to_allocvec(&other).expect("encode")).expect("write");
        assert!(CookInputs::open(&root, "linux").file.stamps.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ADR-2612: the shared key leaves out what differs per machine. Two copies of one project in
    /// different directories, cooked by executables with different stamps, compute one shared key
    /// (and different local keys); another engine build id or any input changes it.
    #[test]
    fn the_shared_key_is_the_same_on_every_machine() {
        let make = |tag: &str| {
            let root = regia_test_utils::unique_temp_dir(tag);
            let assets = root.join("assets");
            std::fs::create_dir_all(&assets).expect("dir");
            std::fs::write(assets.join("hull.gltf"), b"{}").expect("write");
            std::fs::write(assets.join("hull.bin"), b"buffer").expect("write");
            std::fs::write(meta_path_for(&assets.join("hull.gltf")), b"(id: 1)").expect("write");
            root
        };
        let (a, b) = (make("shared-key-a"), make("shared-key-b"));
        let keys = |root: &Path, exe_stamp: &str, build_id: &str| {
            let mut inputs = CookInputs::open(root, "linux");
            inputs.build = exe_stamp.to_string();
            let source = root.join("assets/hull.gltf");
            let src = SourceInputs {
                source: &source,
                rel_path: "assets/hull.gltf",
                project_root: root,
                platform: "linux",
                ext: "gltf",
                importer_version: 7,
                revision: "",
            };
            let sidecars = vec![root.join("assets/hull.bin")];
            let shared = inputs
                .shared_key(build_id, &src, &[1; 32], &sidecars, false)
                .expect("shared");
            let fresh = inputs
                .shared_key(build_id, &src, &[1; 32], &sidecars, true)
                .expect("fresh");
            assert_eq!(shared, fresh, "unchanged files key the same read or stamped");
            (
                inputs.input_key(&src, &[1; 32], &sidecars).expect("local"),
                shared,
            )
        };
        let (local_a, shared_a) = keys(&a, "1.0:111:222", "build-x");
        let (local_b, shared_b) = keys(&b, "1.0:333:444", "build-x");
        assert_ne!(local_a, local_b, "the local key holds the executable stamp");
        assert_eq!(shared_a, shared_b, "the shared key does not");
        assert_ne!(shared_a, local_a);
        assert_ne!(keys(&b, "1.0:333:444", "build-y").1, shared_a);
        std::fs::write(b.join("assets/hull.bin"), b"buffer 2").expect("write");
        assert_ne!(keys(&b, "1.0:333:444", "build-x").1, shared_a);
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    /// ADR-2612: a stamp that lies (a side file rewritten with its size and mtime kept) moves the
    /// stamped key but never the fresh one, which is what fetches and publishes shared entries.
    #[test]
    fn a_fresh_shared_key_ignores_a_lying_stamp() {
        let root = regia_test_utils::unique_temp_dir("shared-key-fresh");
        let assets = root.join("assets");
        std::fs::create_dir_all(&assets).expect("dir");
        let source = assets.join("hull.gltf");
        let bin = assets.join("hull.bin");
        std::fs::write(&source, b"{}").expect("write");
        std::fs::write(&bin, b"buffer").expect("write");
        std::fs::write(meta_path_for(&source), b"(id: 1)").expect("write");
        let src = SourceInputs {
            source: &source,
            rel_path: "assets/hull.gltf",
            project_root: &root,
            platform: "linux",
            ext: "gltf",
            importer_version: 7,
            revision: "",
        };
        let sidecars = vec![bin.clone()];
        let mut clean = CookInputs::open(&root, "linux");
        let truth = clean
            .shared_key("build-x", &src, &[1; 32], &sidecars, true)
            .expect("key");

        let mut lied_to = CookInputs::open(&root, "linux");
        let meta = std::fs::metadata(&bin).expect("meta");
        let mtime = mtime_ns(&meta).expect("mtime");
        lied_to.file.stamps.insert(
            absolute(&bin),
            Stamp {
                len: meta.len(),
                mtime_ns: mtime,
                hashed_at_ns: mtime.saturating_add(RACY_MARGIN_NS * 4),
                hash: [7; 32],
            },
        );
        let stamped = lied_to
            .shared_key("build-x", &src, &[1; 32], &sidecars, false)
            .expect("key");
        assert_ne!(stamped, truth, "the stamped key trusts the lie");
        let fresh = lied_to
            .shared_key("build-x", &src, &[1; 32], &sidecars, true)
            .expect("key");
        assert_eq!(fresh, truth, "the fresh key reads the file");
        // The fresh read replaced the lying stamp.
        assert_eq!(
            lied_to
                .shared_key("build-x", &src, &[1; 32], &sidecars, false)
                .expect("key"),
            truth
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ADR-3321: the import rules a source matches are part of its cook key (and the shared key).
    /// Adding a rules file, or editing a rule, moves exactly the matching sources' keys; a rule
    /// that matches nothing moves none; deleting the file restores the keys.
    #[test]
    fn import_rules_move_the_keys_of_exactly_the_matching_sources() {
        let root = regia_test_utils::unique_temp_dir("cook-inputs-rules");
        let assets = root.join("assets");
        std::fs::create_dir_all(assets.join("ui")).expect("dir");
        std::fs::create_dir_all(assets.join("props")).expect("dir");
        let ui = assets.join("ui/a.png");
        let props = assets.join("props/b.png");
        std::fs::write(&ui, b"ui").expect("write");
        std::fs::write(&props, b"props").expect("write");
        std::fs::write(meta_path_for(&ui), b"(id: 1)").expect("meta");
        std::fs::write(meta_path_for(&props), b"(id: 2)").expect("meta");
        let rules_file = root.join(crate::import_rules::IMPORT_RULES_FILE);
        std::fs::create_dir_all(rules_file.parent().expect("parent")).expect("dir");

        fn src<'a>(root: &'a Path, path: &'a Path, rel: &'a str) -> SourceInputs<'a> {
            SourceInputs {
                source: path,
                rel_path: rel,
                project_root: root,
                platform: "linux",
                ext: "png",
                importer_version: 7,
                revision: "",
            }
        }
        let keys = |root: &Path| {
            let mut inputs = CookInputs::open(root, "linux");
            let ui_key = inputs
                .input_key(&src(root, &ui, "assets/ui/a.png"), &[1; 32], &[])
                .expect("ui key");
            let props_key = inputs
                .input_key(&src(root, &props, "assets/props/b.png"), &[1; 32], &[])
                .expect("props key");
            let shared = inputs
                .shared_key("build-x", &src(root, &ui, "assets/ui/a.png"), &[1; 32], &[], true)
                .expect("shared key");
            (ui_key, props_key, shared)
        };

        // No rules file: the baseline keys.
        let (ui_0, props_0, shared_0) = keys(&root);
        assert_eq!(keys(&root), (ui_0, props_0, shared_0));

        // A rule matching only assets/ui/** moves the ui key (and the shared key), not props'.
        std::fs::write(
            &rules_file,
            "(rules: [(glob: \"assets/ui/**\", settings: \"(compress: false)\")])",
        )
        .expect("rules");
        let (ui_1, props_1, shared_1) = keys(&root);
        assert_ne!(ui_1, ui_0, "a matching rule enters the key");
        assert_eq!(props_1, props_0, "a rule that does not match changes nothing");
        assert_ne!(shared_1, shared_0);
        assert_ne!(shared_1, ui_1, "the shared key is its own kind");

        // An importer filter the source fails also leaves the key alone.
        std::fs::write(
            &rules_file,
            "(rules: [(glob: \"assets/ui/**\", importer: \"gltf\", settings: \"(compress: false)\")])",
        )
        .expect("rules");
        assert_eq!(keys(&root), (ui_0, props_0, shared_0), "no match: no key change");

        // Editing the matched rule moves the ui key again; adding a rule that matches nothing
        // does not move any key (only matched rules are hashed).
        std::fs::write(
            &rules_file,
            "(rules: [(glob: \"assets/ui/**\", settings: \"(compress: true)\")])",
        )
        .expect("rules");
        let (ui_2, props_2, _) = keys(&root);
        assert_ne!(ui_2, ui_1);
        assert_eq!(props_2, props_0);
        std::fs::write(
            &rules_file,
            "(rules: [(glob: \"assets/none/**\", settings: \"(compress: false)\"), (glob: \"assets/ui/**\", settings: \"(compress: true)\")])",
        )
        .expect("rules");
        let (ui_3, props_3, _) = keys(&root);
        assert_eq!(ui_3, ui_2, "a rule that matches nothing is not hashed");
        assert_eq!(props_3, props_0);

        // Deleting the file restores the baseline keys (nothing matched is hashed).
        std::fs::remove_file(&rules_file).expect("remove rules");
        assert_eq!(keys(&root), (ui_0, props_0, shared_0));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_key_changes_with_each_input() {
        let root = regia_test_utils::unique_temp_dir("cook-inputs-key");
        let assets = root.join("assets");
        std::fs::create_dir_all(&assets).expect("dir");
        let source = assets.join("hull.gltf");
        let bin = assets.join("hull.bin");
        std::fs::write(&source, b"{}").expect("write");
        std::fs::write(&bin, b"buffer").expect("write");
        let mut inputs = CookInputs::open(&root, "linux");
        let src = |platform: &'static str| SourceInputs {
            source: &source,
            rel_path: "assets/hull.gltf",
            project_root: &root,
            platform,
            ext: "gltf",
            importer_version: 7,
            revision: "",
        };
        let hash = [1u8; 32];
        let sidecars = vec![bin.clone()];
        assert_eq!(
            inputs.input_key(&src("linux"), &hash, &sidecars),
            None,
            "no .meta yet"
        );

        std::fs::write(meta_path_for(&source), b"(id: 1)").expect("write");
        let base = inputs.input_key(&src("linux"), &hash, &sidecars).expect("key");
        assert_eq!(inputs.input_key(&src("linux"), &hash, &sidecars), Some(base));
        assert_ne!(inputs.input_key(&src("linux"), &[2; 32], &sidecars), Some(base));
        assert_ne!(inputs.input_key(&src("android"), &hash, &sidecars), Some(base));
        assert_ne!(inputs.input_key(&src("linux"), &hash, &[]), Some(base));

        std::fs::write(&bin, b"buffer 2").expect("write");
        let sidecar_edit = inputs.input_key(&src("linux"), &hash, &sidecars).expect("key");
        assert_ne!(sidecar_edit, base);

        std::fs::write(meta_path_for(&source), b"(id: 2)").expect("write");
        let meta_edit = inputs.input_key(&src("linux"), &hash, &sidecars).expect("key");
        assert_ne!(meta_edit, sidecar_edit);

        std::fs::write(root.join(crate::lod_classes::LOD_CLASSES_FILE), b"[classes]").expect("write");
        assert_ne!(inputs.input_key(&src("linux"), &hash, &sidecars), Some(meta_edit));
        let _ = std::fs::remove_dir_all(&root);
    }
}
