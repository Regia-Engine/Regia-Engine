// SPDX-License-Identifier: MPL-2.0
//! `chunk-why.bin` (format `chunk-why` v1): why the cook put each asset in its chunk, so a running
//! game answers `content.why` with the cook's reason chain without the asset pipeline
//! (`docs/specs/40-content-chunks-dlc-asset-packs.md` § 2.2 step 5, § 5; ADR-1876).
//!
//! The cook writes it next to `chunk-map.bin`; packaging stages it with the manifest. It is read
//! lazily (on the first `content.why`), never at startup, and a build without it still answers
//! the chunk from the chunk map with no reason.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::ContentError;
use super::manifest::write_atomic;
use crate::id::AssetId;

/// Version of `chunk-why.bin` this build reads and writes.
pub const CHUNK_WHY_FORMAT_VERSION: u32 = 1;
/// File name of the reason table, next to the chunk map.
pub const CHUNK_WHY_FILE: &str = "chunk-why.bin";
/// First bytes of a reason table file.
pub const CHUNK_WHY_MAGIC: [u8; 8] = *b"RGCHWHY\0";
/// Largest decoded table: the file names its own size, and the decoder allocates what it says.
const MAX_CHUNK_WHY_BYTES: usize = 64 * 1024 * 1024;

/// The serde field-list fingerprint of `chunk-why.bin` (hashed by `cargo xtask check-formats`).
#[must_use]
pub fn chunk_why_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<ChunkWhy>(),
        regia_core::schema::serde_field_names::<WhyRecord>(),
    ])
}

/// Why one asset is in its chunk (the cook's `Reason`, spec 40 § 2.2 step 5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WhyReason {
    /// Selected by its chunk's rule; the index counts the rule's `include` globs, then its `tags`.
    Rule(u32),
    /// Reached from a direct asset; the first referrer on the shortest path.
    Follows(AssetId),
    /// Claimed with equal priority by several chunks.
    Shared,
    /// Reachable from a pinned chunk or the startup level.
    Pinned,
    /// Selected by no rule.
    Default,
}

impl WhyReason {
    /// `rule`, `follows`, `shared`, `pinned` or `default` (the agent's `reason` field).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Rule(_) => "rule",
            Self::Follows(_) => "follows",
            Self::Shared => "shared",
            Self::Pinned => "pinned",
            Self::Default => "default",
        }
    }
}

/// One asset of the reason table.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhyRecord {
    /// Asset id.
    pub id: AssetId,
    /// Project path (`assets/...`).
    pub path: String,
    /// Index into [`ChunkWhy::chunks`] of the asset's chunk.
    pub chunk: u32,
    /// Why it is there.
    pub reason: WhyReason,
    /// With `shared = "duplicate"`: indices of every chunk holding a copy.
    pub duplicated_in: Vec<u32>,
}

/// `chunk-why.bin`. Layout: [`CHUNK_WHY_MAGIC`], the version as little-endian `u32`, then the
/// lz4 block (size-prefixed) of the postcard body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkWhy {
    /// Chunk names; records index into this list.
    pub chunks: Vec<String>,
    /// Each chunk's rule selectors in rule-index order: its `include` globs, then `tag:<tag>`.
    pub selectors: BTreeMap<String, Vec<String>>,
    /// One record per cooked asset, sorted by id.
    pub records: Vec<WhyRecord>,
}

/// The answer to `content.why` for one asset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WhyAnswer {
    /// Asset id.
    pub id: AssetId,
    /// Asset path.
    pub path: String,
    /// The chunk the asset lives in.
    pub chunk: String,
    /// `rule`, `follows`, `shared`, `pinned`, `default`.
    pub reason: &'static str,
    /// For `rule` (also for the direct asset a `follows` chain ends at): the rule's chunk, the
    /// selector index and its text.
    pub rule: Option<(String, u32, String)>,
    /// Asset paths from this asset back to a direct asset (this asset first).
    pub via: Vec<String>,
    /// Chunks holding a copy (`shared = "duplicate"`).
    pub duplicated_in: Vec<String>,
}

impl ChunkWhy {
    fn record(&self, id: AssetId) -> Option<&WhyRecord> {
        let at = self.records.binary_search_by_key(&id, |r| r.id).ok()?;
        self.records.get(at)
    }

    fn chunk_name(&self, index: u32) -> String {
        usize::try_from(index)
            .ok()
            .and_then(|i| self.chunks.get(i))
            .cloned()
            .unwrap_or_default()
    }

    /// Resolves an asset id (text form) or a path, with or without the `assets/` prefix and with
    /// either slash.
    #[must_use]
    pub fn resolve(&self, asset: &str) -> Option<AssetId> {
        if let Ok(id) = asset.parse::<AssetId>() {
            return self.record(id).map(|r| r.id);
        }
        let path = asset.replace('\\', "/");
        let full = if path.starts_with("assets/") {
            path
        } else {
            format!("assets/{path}")
        };
        self.records.iter().find(|r| r.path == full).map(|r| r.id)
    }

    /// The reason chain for `id`: its chunk, its reason, the referrer path back to a direct asset
    /// (cycle-safe, at most one entry per asset) and the rule that selected that direct asset.
    #[must_use]
    pub fn explain(&self, id: AssetId) -> Option<WhyAnswer> {
        let first = self.record(id)?;
        let mut via = vec![first.path.clone()];
        let mut seen = std::collections::BTreeSet::from([id]);
        let mut at = first;
        while let WhyReason::Follows(next) = &at.reason {
            if !seen.insert(*next) {
                break;
            }
            let Some(rec) = self.record(*next) else {
                via.push(next.to_string());
                break;
            };
            via.push(rec.path.clone());
            at = rec;
        }
        let rule = match &at.reason {
            WhyReason::Rule(index) => {
                let chunk = self.chunk_name(at.chunk);
                let selector = self
                    .selectors
                    .get(&chunk)
                    .and_then(|s| usize::try_from(*index).ok().and_then(|i| s.get(i)))
                    .cloned()
                    .unwrap_or_default();
                Some((chunk, *index, selector))
            }
            _ => None,
        };
        Some(WhyAnswer {
            id,
            path: first.path.clone(),
            chunk: self.chunk_name(first.chunk),
            reason: first.reason.kind(),
            rule,
            via,
            duplicated_in: first.duplicated_in.iter().map(|i| self.chunk_name(*i)).collect(),
        })
    }

    /// Direct assets of `chunk` (selected by its rule) with the selector that picked each, sorted
    /// by path; the editor's Content panel lists these.
    #[must_use]
    pub fn direct_assets(&self, chunk: &str) -> Vec<(String, String)> {
        let Some(index) = self.chunks.iter().position(|c| c == chunk) else {
            return Vec::new();
        };
        let selectors = self.selectors.get(chunk);
        let mut out: Vec<(String, String)> = self
            .records
            .iter()
            .filter(|r| usize::try_from(r.chunk).ok() == Some(index))
            .filter_map(|r| match r.reason {
                WhyReason::Rule(i) => Some((
                    r.path.clone(),
                    selectors
                        .and_then(|s| usize::try_from(i).ok().and_then(|i| s.get(i)))
                        .cloned()
                        .unwrap_or_default(),
                )),
                _ => None,
            })
            .collect();
        out.sort();
        out
    }

    /// Encodes the file bytes.
    ///
    /// # Errors
    /// Postcard encoding failed.
    pub fn to_bytes(&self) -> Result<Vec<u8>, ContentError> {
        let body =
            postcard::to_allocvec(self).map_err(|e| ContentError::Format(format!("chunk why: {e}")))?;
        let packed = lz4_flex::compress_prepend_size(&body);
        let mut bytes = Vec::with_capacity(12 + packed.len());
        bytes.extend_from_slice(&CHUNK_WHY_MAGIC);
        bytes.extend_from_slice(&CHUNK_WHY_FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&packed);
        Ok(bytes)
    }

    /// Decodes file bytes; a newer version is [`ContentError::ManifestVersion`].
    ///
    /// # Errors
    /// Bad magic, newer version, or a body that does not decode.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ContentError> {
        let Some((magic, rest)) = bytes.split_first_chunk::<8>() else {
            return Err(ContentError::Format("chunk why: truncated".into()));
        };
        if *magic != CHUNK_WHY_MAGIC {
            return Err(ContentError::Format("chunk why: bad magic".into()));
        }
        let Some((version, packed)) = rest.split_first_chunk::<4>() else {
            return Err(ContentError::Format("chunk why: truncated".into()));
        };
        let found = u32::from_le_bytes(*version);
        // The format starts at 1: a zero version word is damage, not a current file.
        if found == 0 || found > CHUNK_WHY_FORMAT_VERSION {
            return Err(ContentError::ManifestVersion {
                found,
                supported: CHUNK_WHY_FORMAT_VERSION,
            });
        }
        let declared = packed
            .first_chunk::<4>()
            .map_or(0, |b| u32::from_le_bytes(*b) as usize);
        if declared > MAX_CHUNK_WHY_BYTES {
            return Err(ContentError::Format(format!(
                "chunk why: declares {declared} bytes, more than {MAX_CHUNK_WHY_BYTES}"
            )));
        }
        let body = lz4_flex::decompress_size_prepended(packed)
            .map_err(|e| ContentError::Format(format!("chunk why: {e}")))?;
        let mut table: Self =
            postcard::from_bytes(&body).map_err(|e| ContentError::Format(format!("chunk why: {e}")))?;
        table.records.sort_by_key(|r| r.id);
        table.records.dedup_by_key(|r| r.id);
        Ok(table)
    }

    /// Reads `<dir>/chunk-why.bin`; `Ok(None)` when absent.
    ///
    /// # Errors
    /// IO error other than not-found, or bad bytes.
    pub fn read_from_dir(dir: &Path) -> Result<Option<Self>, ContentError> {
        match std::fs::read(dir.join(CHUNK_WHY_FILE)) {
            Ok(bytes) => Self::from_bytes(&bytes).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ContentError::Io(e.to_string())),
        }
    }

    /// Writes `<dir>/chunk-why.bin` atomically.
    ///
    /// # Errors
    /// Encoding or IO error.
    pub fn write_to_dir(&self, dir: &Path) -> Result<(), ContentError> {
        write_atomic(&dir.join(CHUNK_WHY_FILE), &self.to_bytes()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> AssetId {
        AssetId(regia_core::id::Guid::from_u128(n))
    }

    /// arena: level (rule 0) -> mesh (follows) -> texture (follows); base: ui (default).
    fn table() -> ChunkWhy {
        let rec = |n, path: &str, chunk, reason| WhyRecord {
            id: id(n),
            path: path.into(),
            chunk,
            reason,
            duplicated_in: Vec::new(),
        };
        ChunkWhy {
            chunks: vec!["arena".into(), "base".into()],
            selectors: BTreeMap::from([(
                "arena".to_owned(),
                vec!["levels/arena/**".to_owned(), "tag:arena".to_owned()],
            )]),
            records: vec![
                rec(3, "assets/arena/rock.ktx2", 0, WhyReason::Follows(id(2))),
                rec(1, "assets/levels/arena/main.level.ron", 0, WhyReason::Rule(0)),
                rec(2, "assets/arena/rock.mesh", 0, WhyReason::Follows(id(1))),
                rec(4, "assets/ui/hud.png", 1, WhyReason::Default),
                rec(5, "assets/loop/a", 0, WhyReason::Follows(id(6))),
                rec(6, "assets/loop/b", 0, WhyReason::Follows(id(5))),
            ],
        }
    }

    #[test]
    fn a_table_that_declares_a_huge_size_is_refused_before_it_is_allocated() {
        let mut bytes = CHUNK_WHY_MAGIC.to_vec();
        bytes.extend_from_slice(&CHUNK_WHY_FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        let err = ChunkWhy::from_bytes(&bytes).expect_err("too large");
        assert!(err.to_string().contains("declares"), "{err}");
    }

    #[test]
    fn round_trips_sorted_and_refuses_newer_versions() {
        let t = table();
        let back = ChunkWhy::from_bytes(&t.to_bytes().expect("encode")).expect("decode");
        assert!(back.records.windows(2).all(|w| w[0].id < w[1].id));
        assert_eq!(back.records.len(), 6);
        let mut bytes = t.to_bytes().expect("encode");
        bytes[8..12].copy_from_slice(&(CHUNK_WHY_FORMAT_VERSION + 1).to_le_bytes());
        assert!(matches!(
            ChunkWhy::from_bytes(&bytes),
            Err(ContentError::ManifestVersion { .. })
        ));
        assert!(ChunkWhy::from_bytes(b"RGCHMAP\0\x01\0\0\0").is_err());
        assert!(ChunkWhy::from_bytes(b"short").is_err());
    }

    #[test]
    fn explains_the_chain_to_the_rule() {
        let t = ChunkWhy::from_bytes(&table().to_bytes().expect("encode")).expect("decode");
        let tex = t.resolve("arena/rock.ktx2").expect("path without assets/");
        let a = t.explain(tex).expect("explained");
        assert_eq!((a.chunk.as_str(), a.reason), ("arena", "follows"));
        assert_eq!(
            a.via,
            [
                "assets/arena/rock.ktx2",
                "assets/arena/rock.mesh",
                "assets/levels/arena/main.level.ron"
            ]
        );
        assert_eq!(a.rule, Some(("arena".into(), 0, "levels/arena/**".into())));
        let direct = t.explain(id(1)).expect("direct");
        assert_eq!((direct.reason, direct.via.len()), ("rule", 1));
        let ui = t
            .explain(t.resolve("assets\\ui\\hud.png").expect("backslashes"))
            .expect("ui");
        assert_eq!((ui.chunk.as_str(), ui.reason, ui.rule), ("base", "default", None));
        assert_eq!(t.resolve(&id(4).to_string()), Some(id(4)));
        assert_eq!(t.resolve("assets/nope"), None);
        // A referrer cycle stops instead of looping.
        assert_eq!(t.explain(id(5)).expect("loop").via.len(), 2);
        assert_eq!(
            t.direct_assets("arena"),
            [(
                "assets/levels/arena/main.level.ron".to_owned(),
                "levels/arena/**".to_owned()
            )]
        );
        assert!(t.direct_assets("nope").is_empty());
    }
}
