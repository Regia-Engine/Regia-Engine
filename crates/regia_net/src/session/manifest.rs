// SPDX-License-Identifier: MPL-2.0
//! The content manifest, `net-manifest.ron` (spec 77 § 4.1).
//!
//! Client and server compare gameplay content by hashes only: one hash per logical content pack,
//! one per mounted mod, and a combined `content_hash`. Presentation assets (textures, audio,
//! materials, clips, ...) are never hashed, so a client with another texture pack still joins and a
//! server pak that lacks presentation assets hashes the same as a client pak.
//!
//! `format_version: 1`, generated (never edited by hand). A layout change bumps the version; both
//! sides refuse other versions with `content_mismatch` (detail `manifest_version`).

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::wire::{WireError, WireReader, WireWriter};

/// The `format_version` this build writes and the only one it accepts.
pub const NET_MANIFEST_FORMAT_VERSION: u32 = 1;

/// The pack name of the match rules entry ([`ContentManifest::with_rules`], ADR-8694). A content
/// pack name never starts with `@`.
pub const RULES_PACK: &str = "@rules";

/// At most this many differing items are reported in a mismatch detail.
pub const MAX_MANIFEST_DIFF_ITEMS: usize = 32;

/// At most this many packs or mods travel in a `Hello`.
pub const MAX_MANIFEST_ENTRIES: usize = 1024;

/// The asset kinds whose cooked content is hashed (spec 77 § 4.1): levels, prefabs, net profiles,
/// data tables and other gameplay data, `config/*.ron`, collision meshes, heightfields and navmesh
/// data. Every other kind (presentation) is not hashed.
pub const HASHED_KINDS: &[&str] = &[
    "level",
    "prefab",
    "netprofile",
    "data_table",
    "data",
    "config",
    "collision",
    "heightfield",
    "navmesh",
];

/// True when assets of `kind` are part of the gameplay content hash.
pub fn is_hashed_kind(kind: &str) -> bool {
    HASHED_KINDS.contains(&kind)
}

/// One cooked asset as the manifest builder sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestAsset {
    /// Stable asset id (the project-relative path or id string).
    pub id: String,
    /// Asset kind (see [`HASHED_KINDS`]).
    pub kind: String,
    /// Hash of the cooked content.
    pub content_hash: u64,
}

/// A mounted mod in the in-memory manifest (added at session start, never written to the file).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestMod {
    /// Mod id.
    pub id: String,
    /// Mod version text.
    pub version: String,
    /// Hash of the mod's gameplay content.
    pub hash: u64,
}

/// The content manifest: what a client must match to join (spec 77 § 4.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename = "NetManifest")]
pub struct ContentManifest {
    /// Layout version, [`NET_MANIFEST_FORMAT_VERSION`].
    pub format_version: u32,
    /// Engine version text.
    pub engine_version: String,
    /// Game version text.
    pub game_version: String,
    /// The replication protocol version.
    pub protocol_version: u32,
    /// The protocol hash (code-registered types and codecs).
    pub protocol_hash: u64,
    /// Hash over every pack and mod, in name order.
    pub content_hash: u64,
    /// `(pack name, pack hash)`, sorted by name.
    pub packs: Vec<(String, u64)>,
    /// Mounted mods, sorted by id. In memory only: the file holds the packaged packs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mods: Vec<ManifestMod>,
}

/// Why a manifest could not be read.
#[derive(Debug, Error)]
pub enum ManifestError {
    /// Not valid RON for the manifest shape.
    #[error("net-manifest.ron: {0}")]
    Parse(String),
    /// A `format_version` this build does not read.
    #[error("net-manifest.ron: format_version {0} is not {NET_MANIFEST_FORMAT_VERSION}")]
    Version(u32),
    /// File IO.
    #[error("net-manifest.ron: {0}")]
    Io(String),
}

/// One differing pack or mod in a `content_mismatch` detail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestDiffItem {
    /// `pack:<name>`, `mod:<id>` or `manifest_version`.
    pub item: String,
    /// `missing on client`, `missing on server` or `different`; a mod whose version text differs
    /// says `different (client <version>, server <version>)` (ADR-7259).
    pub status: String,
}

impl std::fmt::Display for ManifestDiffItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.item, self.status)
    }
}

fn hash64(bytes: &[u8]) -> u64 {
    let digest = blake3::hash(bytes);
    let mut out = [0u8; 8];
    out.copy_from_slice(&digest.as_bytes()[..8]);
    u64::from_le_bytes(out)
}

/// Hash of one pack: `(asset id, content hash)` of its hashed-kind assets, sorted by asset id.
/// Presentation kinds are skipped, so the same gameplay assets in another physical pak (or next
/// to other presentation assets) hash the same.
pub fn pack_hash(assets: &[ManifestAsset]) -> u64 {
    let mut kept: Vec<&ManifestAsset> = assets.iter().filter(|a| is_hashed_kind(&a.kind)).collect();
    kept.sort_by(|a, b| a.id.cmp(&b.id));
    let mut hasher = blake3::Hasher::new_derive_key("regia_net net-manifest pack v1");
    for a in kept {
        hasher.update(&(a.id.len() as u64).to_le_bytes());
        hasher.update(a.id.as_bytes());
        hasher.update(&a.content_hash.to_le_bytes());
    }
    let mut out = [0u8; 8];
    out.copy_from_slice(&hasher.finalize().as_bytes()[..8]);
    u64::from_le_bytes(out)
}

impl ContentManifest {
    /// A manifest from versions and packs (each pack: its name and its cooked assets). Packs and
    /// mods are sorted; `content_hash` is computed.
    pub fn build(
        engine_version: &str,
        game_version: &str,
        protocol_version: u32,
        protocol_hash: u64,
        packs: &[(&str, &[ManifestAsset])],
    ) -> Self {
        let mut m = Self {
            format_version: NET_MANIFEST_FORMAT_VERSION,
            engine_version: engine_version.to_owned(),
            game_version: game_version.to_owned(),
            protocol_version,
            protocol_hash,
            content_hash: 0,
            packs: packs
                .iter()
                .map(|(n, a)| ((*n).to_owned(), pack_hash(a)))
                .collect(),
            mods: Vec::new(),
        };
        m.rehash();
        m
    }

    /// Adds the mounted mods (session start) and recomputes `content_hash`.
    pub fn with_mods(mut self, mods: Vec<ManifestMod>) -> Self {
        self.mods = mods;
        self.rehash();
        self
    }

    /// Adds (or replaces) the match rules entry, the pack [`RULES_PACK`] holding the content hash of
    /// the resolved rule sets (`regia_rules::ruleset::ResolvedRules::content_hash`, ADR-8694), and
    /// recomputes `content_hash`. Peers whose rules differ then differ in that pack and the server
    /// refuses the client with `content_mismatch` (`pack:@rules: different`). The manifest file
    /// layout is unchanged: the entry is one more `(name, hash)` pack.
    pub fn with_rules(mut self, rules_hash: u64) -> Self {
        self.packs.retain(|(name, _)| name != RULES_PACK);
        self.packs.push((RULES_PACK.to_owned(), rules_hash));
        self.rehash();
        self
    }

    /// Sorts packs and mods and recomputes `content_hash` over every pack and mod in name order.
    pub fn rehash(&mut self) {
        self.packs.sort_by(|a, b| a.0.cmp(&b.0));
        self.mods.sort_by(|a, b| a.id.cmp(&b.id));
        let mut w = WireWriter::new();
        for (name, hash) in &self.packs {
            w.raw(b"P");
            w.u64(name.len() as u64);
            w.raw(name.as_bytes());
            w.u64(*hash);
        }
        for m in &self.mods {
            w.raw(b"M");
            w.u64(m.id.len() as u64);
            w.raw(m.id.as_bytes());
            w.u64(m.version.len() as u64);
            w.raw(m.version.as_bytes());
            w.u64(m.hash);
        }
        self.content_hash = hash64(&w.finish());
    }

    /// Parses `net-manifest.ron` text. Refuses any version but [`NET_MANIFEST_FORMAT_VERSION`].
    pub fn from_ron(text: &str) -> Result<Self, ManifestError> {
        #[derive(Deserialize)]
        struct Peek {
            format_version: u32,
        }
        let peek: Peek = ron::from_str::<ron::Value>(text)
            .map_err(|e| ManifestError::Parse(e.to_string()))
            .and_then(|v| {
                v.into_rust::<Peek>()
                    .map_err(|e| ManifestError::Parse(e.to_string()))
            })?;
        if peek.format_version != NET_MANIFEST_FORMAT_VERSION {
            return Err(ManifestError::Version(peek.format_version));
        }
        ron::from_str(text).map_err(|e| ManifestError::Parse(e.to_string()))
    }

    /// The file text (packs only; mods are never written). `\n` on every platform: ron's default
    /// is `\r\n` on Windows (ADR-4772, same fix as `KnownHosts::to_ron`, ADR-3150).
    pub fn to_ron(&self) -> Result<String, ManifestError> {
        let mut file = self.clone();
        file.mods.clear();
        file.rehash();
        let config = regia_core::ron_pretty().struct_names(true);
        ron::ser::to_string_pretty(&file, config)
            .map(|mut s| {
                s.push('\n');
                s
            })
            .map_err(|e| ManifestError::Parse(e.to_string()))
    }

    /// Reads the file (packaged builds: `content/net-manifest.ron`).
    pub fn load(path: &std::path::Path) -> Result<Self, ManifestError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ManifestError::Io(format!("{}: {e}", path.display())))?;
        Self::from_ron(&text)
    }

    /// Compares a client's manifest (`self`) with the server's. Empty when they match. Constant
    /// time in the number of assets: only pack and mod hashes are compared.
    pub fn diff(&self, server: &ContentManifest) -> Vec<ManifestDiffItem> {
        let mut out = Vec::new();
        if self.format_version != server.format_version {
            out.push(ManifestDiffItem {
                item: "manifest_version".into(),
                status: format!("client {} server {}", self.format_version, server.format_version),
            });
            return out;
        }
        if self.content_hash == server.content_hash && self.packs == server.packs && self.mods == server.mods
        {
            return out;
        }
        let mut items: Vec<(String, String)> = Vec::new();
        diff_sorted(
            &self.packs,
            &server.packs,
            |p| p.0.as_str(),
            |a, b| a.1 == b.1,
            |_, _| None,
            "pack:",
            &mut items,
        );
        diff_sorted(
            &self.mods,
            &server.mods,
            |m| m.id.as_str(),
            |a, b| a.hash == b.hash && a.version == b.version,
            // Both versions when they differ, so a player sees which one to install (ADR-7259).
            |c, s| {
                (c.version != s.version).then(|| {
                    format!(
                        "different (client {}, server {})",
                        version_text(&c.version),
                        version_text(&s.version)
                    )
                })
            },
            "mod:",
            &mut items,
        );
        items.sort();
        if items.is_empty() {
            // Same entries but another combined hash: a client computed it differently.
            items.push(("content_hash".into(), "different".into()));
        }
        out.extend(
            items
                .into_iter()
                .take(MAX_MANIFEST_DIFF_ITEMS)
                .map(|(item, status)| ManifestDiffItem { item, status }),
        );
        out
    }

    /// Writes the manifest into a `Hello`.
    pub fn encode(&self, w: &mut WireWriter) -> Result<(), WireError> {
        w.u32(self.format_version);
        w.str_u8(&self.engine_version, 64)?;
        w.str_u8(&self.game_version, 64)?;
        w.u32(self.protocol_version);
        w.u64(self.protocol_hash);
        w.u64(self.content_hash);
        let longest = self.packs.len().max(self.mods.len());
        if longest > MAX_MANIFEST_ENTRIES {
            return Err(WireError::TooLong {
                len: longest,
                max: MAX_MANIFEST_ENTRIES,
            });
        }
        w.u16(self.packs.len() as u16);
        for (name, hash) in &self.packs {
            w.str_u8(name, 128)?;
            w.u64(*hash);
        }
        w.u16(self.mods.len() as u16);
        for m in &self.mods {
            w.str_u8(&m.id, 128)?;
            w.str_u8(&m.version, 64)?;
            w.u64(m.hash);
        }
        Ok(())
    }

    /// Reads a manifest written by [`Self::encode`].
    pub fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        let format_version = r.u32()?;
        let engine_version = r.str_u8(64)?.to_owned();
        let game_version = r.str_u8(64)?.to_owned();
        let protocol_version = r.u32()?;
        let protocol_hash = r.u64()?;
        let content_hash = r.u64()?;
        let packs_len = usize::from(r.u16()?);
        if packs_len > MAX_MANIFEST_ENTRIES {
            return Err(WireError::TooLong {
                len: packs_len,
                max: MAX_MANIFEST_ENTRIES,
            });
        }
        let mut packs = Vec::with_capacity(packs_len);
        for _ in 0..packs_len {
            let name = r.str_u8(128)?.to_owned();
            packs.push((name, r.u64()?));
        }
        let mods_len = usize::from(r.u16()?);
        if mods_len > MAX_MANIFEST_ENTRIES {
            return Err(WireError::TooLong {
                len: mods_len,
                max: MAX_MANIFEST_ENTRIES,
            });
        }
        let mut mods = Vec::with_capacity(mods_len);
        for _ in 0..mods_len {
            let id = r.str_u8(128)?.to_owned();
            let version = r.str_u8(64)?.to_owned();
            mods.push(ManifestMod {
                id,
                version,
                hash: r.u64()?,
            });
        }
        Ok(Self {
            format_version,
            engine_version,
            game_version,
            protocol_version,
            protocol_hash,
            content_hash,
            packs,
            mods,
        })
    }
}

/// A mod's version in a mismatch status: the text, or `no version` when the mod declares none.
fn version_text(version: &str) -> &str {
    if version.is_empty() { "no version" } else { version }
}

fn diff_sorted<T>(
    client: &[T],
    server: &[T],
    key: impl Fn(&T) -> &str,
    same: impl Fn(&T, &T) -> bool,
    explain: impl Fn(&T, &T) -> Option<String>,
    prefix: &str,
    out: &mut Vec<(String, String)>,
) {
    // Indexed, so a hostile manifest of 1024 entries costs 1024 lookups rather than a million
    // string compares per `Hello`. The first entry of a repeated key wins, as a scan would have it.
    let mut by_key: HashMap<&str, &T> = HashMap::with_capacity(client.len());
    for c in client {
        by_key.entry(key(c)).or_insert(c);
    }
    let server_keys: HashSet<&str> = server.iter().map(&key).collect();
    for s in server {
        match by_key.get(key(s)) {
            None => out.push((format!("{prefix}{}", key(s)), "missing on client".into())),
            Some(c) if !same(c, s) => out.push((
                format!("{prefix}{}", key(s)),
                explain(c, s).unwrap_or_else(|| "different".into()),
            )),
            Some(_) => {}
        }
    }
    for c in client {
        if !server_keys.contains(key(c)) {
            out.push((format!("{prefix}{}", key(c)), "missing on server".into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit fix-net3-determinism M3 (ADR-4772): `net-manifest.ron` must save the same bytes on
    /// every host. ron's default newline is `\r\n` on Windows, so this only fails there; the
    /// assertion guards the contract on every platform.
    #[test]
    fn to_ron_uses_lf_on_every_platform() {
        let base = [asset("levels/a.level.ron", "level", 1)];
        let m = ContentManifest::build("0.5.0", "1.2.0", 1, 0x8f3a, &[("base", &base)]);
        let text = m.to_ron().expect("ron");
        assert!(text.contains('\n'), "the text has newlines");
        assert!(!text.contains('\r'), "no CR bytes on any platform");
        assert_eq!(ContentManifest::from_ron(&text).expect("re-parse"), m);
    }

    fn asset(id: &str, kind: &str, h: u64) -> ManifestAsset {
        ManifestAsset {
            id: id.into(),
            kind: kind.into(),
            content_hash: h,
        }
    }

    #[test]
    fn presentation_assets_are_not_hashed_and_order_does_not_matter() {
        let a = [
            asset("levels/a.level.ron", "level", 1),
            asset("t/rock.ktx2", "texture", 2),
        ];
        let b = [
            asset("t/rock_hi.ktx2", "texture", 9),
            asset("levels/a.level.ron", "level", 1),
        ];
        assert_eq!(pack_hash(&a), pack_hash(&b));
        let c = [asset("levels/a.level.ron", "level", 3)];
        assert_ne!(pack_hash(&a), pack_hash(&c));
    }

    #[test]
    fn ron_round_trip_drops_mods_and_refuses_other_versions() {
        let base = [asset("levels/a.level.ron", "level", 1)];
        let m = ContentManifest::build("0.5.0", "1.2.0", 1, 0x8f3a, &[("base", &base)]).with_mods(vec![
            ManifestMod {
                id: "m".into(),
                version: "1".into(),
                hash: 5,
            },
        ]);
        let text = m.to_ron().expect("ron");
        assert!(text.starts_with("NetManifest("), "{text}");
        let back = ContentManifest::from_ron(&text).expect("parse");
        assert!(back.mods.is_empty());
        assert_eq!(back.packs, m.packs);
        let bumped = text.replace("format_version: 1", "format_version: 2");
        assert!(matches!(
            ContentManifest::from_ron(&bumped),
            Err(ManifestError::Version(2))
        ));
        // The spec's example literal (hex integers) parses.
        let example = "NetManifest(format_version: 1, engine_version: \"0.5.0\", game_version: \"1.2.0\", \
            protocol_version: 1, protocol_hash: 0x8f3a51c2d9e04b17, content_hash: 0x0123456789abcdef, \
            packs: [(\"base\", 0x1111111111111111), (\"dlc_harbor\", 0x2222222222222222)])";
        let parsed = ContentManifest::from_ron(example).expect("example");
        assert_eq!(parsed.packs.len(), 2);
    }

    #[test]
    fn wire_round_trip() {
        let base = [asset("levels/a.level.ron", "level", 1)];
        let m = ContentManifest::build("0.5.0", "1.2.0", 1, 7, &[("base", &base), ("dlc", &base)]).with_mods(
            vec![ManifestMod {
                id: "m".into(),
                version: "1".into(),
                hash: 5,
            }],
        );
        let mut w = WireWriter::new();
        m.encode(&mut w).expect("encode");
        let bytes = w.finish();
        let back = ContentManifest::decode(&mut WireReader::new(&bytes)).expect("decode");
        assert_eq!(back, m);
    }
}
