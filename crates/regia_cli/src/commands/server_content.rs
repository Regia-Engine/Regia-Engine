// SPDX-License-Identifier: MPL-2.0
//! What a dedicated server ships (`docs/specs/76-dedicated-server-package.md` § 3, ADR-1886) and the
//! gameplay content manifest both packages write (spec 77 § 4.1).
//!
//! Every asset has a *kind*, from its cooked type or its file name alone, so a client and a server
//! package of one tree agree on it. The server pak is the normal closure walk with a kind filter:
//! the walk runs first (a level reaches a skeleton through a model, which the filter then drops).
//!
//! Three kinds depend on how an asset is used, read from the level and prefab text in the closure:
//!
//! - A mesh is `collision` when a key naming a collider points at it; any other mesh is `mesh`.
//! - A skeleton or clip stays when an entity with `HitShapes { pose_source: Joints }` or a
//!   root-motion component reaches it (spec 76 § 3); the check is per entity.
//!
//! `net-manifest.ron` covers the hashed kinds of the whole project, whatever a profile ships, so
//! neither the filter nor a client profile's entry levels change it (§ 11 test 4). A cooked asset
//! hashes by its source file and `.meta`, not its cooked bytes, which differ per platform.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use regia_asset::{cooked_cache_dir, cooked_file_name};
use regia_net::session::manifest::{ContentManifest, ManifestAsset, is_hashed_kind};
use regia_net::settings::NET_PROTOCOL_VERSION;

use crate::commands::build_profile::{self, Closure, ClosureAsset, ClosureRaw, ResolvedProfile};
use crate::manifest::BuildProfile;

/// The one logical content pack today (`ensure_paks` writes one pak).
pub const BASE_PACK: &str = "base";

/// Kinds a server never ships.
pub const PRESENTATION_KINDS: &[&str] = &[
    "texture",
    "material",
    "shader",
    "audio",
    "font",
    "ui",
    "vfx",
    "video",
    "cinematic",
    "mesh",
    "model",
    "lighting",
    "sprite",
];

/// Kinds a server ships only when an entity needs a pose or root motion from them.
const POSE_KINDS: &[&str] = &["skeleton", "clip"];

/// True for a kind the server pak must not hold ([`PRESENTATION_KINDS`]).
#[must_use]
pub fn is_presentation_kind(kind: &str) -> bool {
    PRESENTATION_KINDS.contains(&kind)
}

/// The kind of a cooked asset from its reflect type path. A type this table does not know (a game's
/// own cooked type) counts as gameplay data.
#[must_use]
pub fn cooked_kind(type_path: &str) -> &'static str {
    match type_path {
        "regia_asset::CookedTexture" | "regia_asset::CookedImage" => "texture",
        "regia_asset::CookedStandardMaterial"
        | "regia_asset::MaterialVariantSet"
        | regia_matgraph::COOKED_MATGRAPH_TYPE_PATH => "material",
        "regia_asset::CookedMesh"
        | "regia_asset::CpuMesh"
        | "regia_asset::GpuMesh"
        | "regia_render::MeshletMesh" => "mesh",
        "regia_asset::CookedModel"
        | "regia_asset::LodGroup"
        | "regia_asset::PrefabSocketSet"
        | regia_asset::PREFAB_INSTANCES_TYPE_PATH => "model",
        "regia_animation::Skeleton" => "skeleton",
        "regia_animation::AnimationClip" => "clip",
        "regia_asset::Heightfield"
        | regia_terrain::stream::TILED_HEIGHTFIELD_TYPE_PATH
        | regia_terrain::stream::TERRAIN_INDEX_TYPE_PATH => "heightfield",
        "regia_asset::lightmap::Lightmap"
        | "regia_asset::baked_gi::BakedGiVolume"
        | "regia_terrain::ScatterSet" => "lighting",
        "regia_2d::CookedTileMap" | "regia_2d::SpriteAtlas" => "sprite",
        // A cooked sound is its loudness record (spec 95); the source stays a raw `audio` file.
        regia_asset_pipeline::audio_cook::SOUND_LOUDNESS_TYPE
        | regia_asset_pipeline::audio_cook::COOKED_SOUND_TYPE => "audio",
        _ => "data",
    }
}

/// The kind of a raw file (a path under `assets/`) from its name. A file this table does not know
/// counts as gameplay data: a server that carries a stray text file costs bytes, one that lacks a
/// data file does not run.
#[must_use]
pub fn raw_kind(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    let ext = name.rsplit_once('.').map_or("", |(_, e)| e);
    let suffix_kinds: &[(&str, &'static str)] = &[
        (".level.ron", "level"),
        (".prefab.ron", "prefab"),
        (".netprofile.ron", "netprofile"),
        (".table.ron", "data_table"),
        (".table.csv", "data_table"),
        (".navmesh.ron", "navmesh"),
        (".material.ron", "material"),
        (".matgraph.ron", "material"),
        (".matinst.ron", "material"),
        (".vfx.ron", "vfx"),
        (".decal.ron", "vfx"),
        (".ui.ron", "ui"),
        (".theme.ron", "ui"),
        (".uisettings.ron", "ui"),
        (".atlas.ron", "sprite"),
        (".tilemap.ron", "sprite"),
        (".autotile.ron", "sprite"),
        (".cue.ron", "audio"),
        (".audiopatch.ron", "audio"),
        (".music.ron", "audio"),
        (".ambience.ron", "audio"),
        (".shots.ron", "cinematic"),
        (".bake.ron", "lighting"),
        (".scatter.ron", "lighting"),
        (".animgraph.ron", "clip"),
        (".montage.ron", "clip"),
    ];
    if let Some((_, kind)) = suffix_kinds.iter().find(|(suffix, _)| name.ends_with(suffix)) {
        return kind;
    }
    if path.starts_with("config/") && ext == "ron" {
        return "config";
    }
    match ext {
        "png" | "jpg" | "jpeg" | "ktx2" | "dds" | "hdr" | "exr" | "tga" | "bmp" => "texture",
        "wav" | "ogg" | "flac" | "mp3" | "opus" => "audio",
        "ttf" | "otf" | "woff" | "woff2" => "font",
        "wgsl" | "glsl" | "hlsl" | "spv" => "shader",
        "ftl" => "ui",
        "webm" | "mp4" | "mkv" | "ivf" => "video",
        "glb" | "gltf" | "fbx" | "obj" | "usd" | "usda" | "usdc" | "usdz" => "mesh",
        "navmesh" => "navmesh",
        _ => "data",
    }
}

/// One RON token.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token<'a> {
    Open,
    Close,
    Colon,
    Comma,
    Str(&'a str),
    Word(&'a str),
}

/// A small RON tokenizer: enough to find keys, their values and entity boundaries without knowing
/// any component type (a game's own components parse too). Skips comments and escapes; a broken
/// file gives fewer tokens, never an error or a panic.
fn tokens(text: &str) -> Vec<Token<'_>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                let start = i + 1;
                let mut j = start;
                while j < bytes.len() && bytes[j] != b'"' {
                    j += if bytes[j] == b'\\' { 2 } else { 1 };
                }
                let Some(body) = text.get(start..j.min(bytes.len())) else {
                    break;
                };
                out.push(Token::Str(body));
                i = j + 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b'(' | b'[' | b'{' => {
                out.push(Token::Open);
                i += 1;
            }
            b')' | b']' | b'}' => {
                out.push(Token::Close);
                i += 1;
            }
            b':' => {
                // `::` belongs to a path, not to a key.
                if bytes.get(i + 1) == Some(&b':') {
                    i += 2;
                } else {
                    out.push(Token::Colon);
                    i += 1;
                }
            }
            b',' => {
                out.push(Token::Comma);
                i += 1;
            }
            c if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b'-' || c == b'+' => {
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'.' | b'-' | b'+'))
                {
                    i += 1;
                }
                if let Some(word) = text.get(start..i) {
                    out.push(Token::Word(word));
                }
            }
            _ => i += 1,
        }
    }
    out
}

/// The token range of the value that starts at `at`: one token, or a balanced group.
fn value_end(tokens: &[Token<'_>], at: usize) -> usize {
    let mut depth = 0usize;
    let mut i = at;
    while i < tokens.len() {
        match tokens[i] {
            Token::Open => depth += 1,
            Token::Close => {
                if depth <= 1 {
                    return i + 1;
                }
                depth -= 1;
            }
            Token::Comma if depth == 0 => return i,
            _ => {}
        }
        i += 1;
    }
    tokens.len()
}

/// The entities of a level or prefab: the elements of its `entities` list, each as a token slice.
/// A file without one is one entity.
fn entity_scopes<'a, 't>(tokens: &'t [Token<'a>]) -> Vec<&'t [Token<'a>]> {
    let key = tokens
        .windows(3)
        .position(|w| w[0] == Token::Word("entities") && w[1] == Token::Colon && w[2] == Token::Open);
    let Some(key) = key else {
        return vec![tokens];
    };
    let list_end = value_end(tokens, key + 2).saturating_sub(1);
    let mut scopes = Vec::new();
    let mut i = key + 3;
    while i < list_end {
        if tokens[i] == Token::Comma {
            i += 1;
            continue;
        }
        let end = value_end(tokens, i).min(list_end);
        scopes.push(&tokens[i..end]);
        i = end.max(i + 1);
    }
    scopes
}

/// True when the key token at `i` is followed by a colon.
fn is_key(scope: &[Token<'_>], i: usize) -> bool {
    scope.get(i + 1) == Some(&Token::Colon)
}

/// The strings inside the values of every key whose name contains `needle` (case-insensitive).
fn strings_under_key<'a>(scope: &[Token<'a>], needle: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    for (i, token) in scope.iter().enumerate() {
        let (Token::Str(name) | Token::Word(name)) = token else {
            continue;
        };
        if !is_key(scope, i) || !name.to_ascii_lowercase().contains(needle) {
            continue;
        }
        let end = value_end(scope, i + 2);
        out.extend(scope[(i + 2).min(end)..end].iter().filter_map(|t| match t {
            Token::Str(s) => Some(*s),
            _ => None,
        }));
    }
    out
}

/// True when this entity needs a pose or root motion on the server: a `HitShapes` value that names
/// `Joints`, or a root-motion component.
fn needs_pose(scope: &[Token<'_>]) -> bool {
    scope.iter().enumerate().any(|(i, token)| {
        let (Token::Str(name) | Token::Word(name)) = token else {
            return false;
        };
        if !is_key(scope, i) {
            return false;
        }
        if name.contains("RootMotion") {
            return true;
        }
        if name.contains("HitShapes") {
            let end = value_end(scope, i + 2);
            return scope[(i + 2).min(end)..end]
                .iter()
                .any(|t| *t == Token::Word("Joints") || *t == Token::Str("Joints"));
        }
        false
    })
}

/// How the levels and prefabs of a closure use their assets.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AssetUse {
    /// Normalized paths of meshes a collider points at.
    pub collision: BTreeSet<String>,
    /// Strings on entities that need a pose: the roots of the skeleton and clip walk.
    pub pose_roots: BTreeSet<String>,
}

/// True for a string that may name an asset: a path, or `asset:<id>`.
fn is_reference(text: &str) -> bool {
    text.contains('/') || text.contains('.') || text.starts_with("asset:")
}

/// Reads one level or prefab text.
fn scan_use(text: &str, into: &mut AssetUse) {
    let tokens = tokens(text);
    for scope in entity_scopes(&tokens) {
        for needle in ["collider", "collision"] {
            into.collision
                .extend(strings_under_key(scope, needle).into_iter().map(normalize_ref));
        }
        if needs_pose(scope) {
            into.pose_roots.extend(scope.iter().filter_map(|t| match t {
                Token::Str(s) if is_reference(s) => Some(normalize_ref(s)),
                _ => None,
            }));
        }
    }
}

/// A reference without its `assets://` / `assets/` prefix and `#sub-asset` suffix. `asset:<id>`
/// keeps only the lowercase id, which [`asset_use`] turns into a path.
fn normalize_ref(text: &str) -> String {
    if let Some(id) = text.strip_prefix("asset:") {
        return id.to_lowercase();
    }
    let file = text.split('#').next().unwrap_or(text).replace('\\', "/");
    let file = file.strip_prefix("assets://").unwrap_or(&file);
    let file = file.strip_prefix("./").unwrap_or(file);
    file.strip_prefix("assets/").unwrap_or(file).to_owned()
}

/// The use of every level and prefab of `closure`.
fn asset_use(project_dir: &Path, closure: &Closure) -> Result<AssetUse> {
    let mut out = AssetUse::default();
    for raw in &closure.raw {
        // A split level (`<x>.level.d/level.ron` and its `entities/**/<guid>.entity.ron`) holds
        // the same components as a one-file level.
        let in_level_dir = raw
            .path
            .split('/')
            .any(|part| part.ends_with(regia_scene::level_dir::LEVEL_DIR_SUFFIX))
            && raw.path.ends_with(".ron");
        if !in_level_dir && !matches!(raw_kind(&raw.path), "level" | "prefab") {
            continue;
        }
        let path = project_dir.join("assets").join(&raw.path);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading gameplay uses from {}", path.display()))?;
        scan_use(&text, &mut out);
    }
    // References by asset id become the paths everything else compares.
    let by_id: BTreeMap<String, String> = closure
        .assets
        .iter()
        .map(|a| (a.id.to_string().to_lowercase(), normalize_ref(&a.path)))
        .collect();
    for set in [&mut out.collision, &mut out.pose_roots] {
        *set = set
            .iter()
            .map(|r| by_id.get(r).cloned().unwrap_or_else(|| r.clone()))
            .collect();
    }
    Ok(out)
}

/// The kind of a cooked closure asset, given how the levels use it.
fn cooked_use_kind(asset: &ClosureAsset, uses: &AssetUse) -> &'static str {
    let kind = cooked_kind(&asset.type_path);
    if kind == "mesh" && uses.collision.contains(&normalize_ref(&asset.path)) {
        "collision"
    } else {
        kind
    }
}

/// The assets the pose walk reaches: the closure of `uses.pose_roots`. Empty without roots.
fn pose_closure(
    project_dir: &Path,
    platform: &str,
    closure: &Closure,
    uses: &AssetUse,
) -> Result<BTreeSet<String>> {
    let cooked: BTreeSet<String> = closure.assets.iter().map(|a| normalize_ref(&a.path)).collect();
    let roots: Vec<String> = uses
        .pose_roots
        .iter()
        .filter(|root| cooked.contains(*root) || closure.raw.iter().any(|r| r.path == **root))
        .cloned()
        .collect();
    if roots.is_empty() {
        return Ok(BTreeSet::new());
    }
    let spec = BuildProfile {
        entry_levels: roots,
        ..BuildProfile::default()
    };
    let reached = build_profile::compute_closure(project_dir, platform, Some(&spec))?;
    Ok(reached
        .assets
        .iter()
        .map(|a| normalize_ref(&a.path))
        .chain(reached.raw.iter().map(|raw| normalize_ref(&raw.path)))
        .collect())
}

/// A closure split into what the server ships and what the kind filter removed.
#[derive(Debug, Clone, Default)]
pub struct ServerContent {
    /// The server's closure: its cooked assets go into `content/base.fpak`, its raw files into
    /// `assets/`.
    pub kept: Closure,
    /// Cooked assets the kind filter removed, with their kind.
    pub filtered_assets: Vec<(ClosureAsset, &'static str)>,
    /// Raw files the kind filter removed, with their kind.
    pub filtered_raw: Vec<(ClosureRaw, &'static str)>,
    /// Kept cooked assets by path, with their kind.
    pub kept_kinds: BTreeMap<String, &'static str>,
}

/// Applies the kind filter to `closure` (spec 76 § 3).
///
/// # Errors
///
/// A level or prefab could not be read, or the pose walk could not read a cooked file.
pub fn filter_closure(project_dir: &Path, platform: &str, closure: &Closure) -> Result<ServerContent> {
    let uses = asset_use(project_dir, closure)?;
    let posed = pose_closure(project_dir, platform, closure, &uses)?;
    let mut out = ServerContent::default();
    out.kept.excluded_assets = closure.excluded_assets.clone();
    out.kept.excluded_raw = closure.excluded_raw.clone();
    for asset in &closure.assets {
        let kind = cooked_use_kind(asset, &uses);
        let keep = if POSE_KINDS.contains(&kind) {
            posed.contains(&normalize_ref(&asset.path))
        } else {
            !is_presentation_kind(kind)
        };
        if keep {
            out.kept_kinds.insert(asset.path.clone(), kind);
            out.kept.assets.push(asset.clone());
        } else {
            out.filtered_assets.push((asset.clone(), kind));
        }
    }
    for raw in &closure.raw {
        let kind = raw_kind(&raw.path);
        let keep = if POSE_KINDS.contains(&kind) {
            posed.contains(&normalize_ref(&raw.path))
        } else {
            !is_presentation_kind(kind)
        };
        if keep {
            out.kept.raw.push(raw.clone());
        } else {
            out.filtered_raw.push((raw.clone(), kind));
        }
    }
    Ok(out)
}

/// The `--dry-run` report of a server package: what is kept and what the filter removed.
#[must_use]
pub fn dry_run_report(profile: &ResolvedProfile, platform: &str, content: &ServerContent) -> String {
    let mut out = build_profile::size_report(profile, platform, &content.kept);
    out.push_str(&format!(
        "\nserver kind filter removed ({} cooked, {} raw):\n",
        content.filtered_assets.len(),
        content.filtered_raw.len()
    ));
    for (asset, kind) in &content.filtered_assets {
        out.push_str(&format!("  {:>12} B  {}  [{kind}]\n", asset.bytes, asset.path));
    }
    for (raw, kind) in &content.filtered_raw {
        out.push_str(&format!("  {:>12} B  {}  [{kind}]\n", raw.bytes, raw.path));
    }
    out
}

fn hash64(bytes: &[u8]) -> u64 {
    let digest = blake3::hash(bytes);
    let mut out = [0u8; 8];
    out.copy_from_slice(&digest.as_bytes()[..8]);
    u64::from_le_bytes(out)
}

/// Feeds one tagged part to `hasher`: the tag and the data are length-prefixed and a missing part
/// is its own marker, so no two different part lists hash the same bytes (audit wave 5, netplug M1:
/// `file || meta` let a file holding both halves collide with the pair).
fn framed(hasher: &mut blake3::Hasher, tag: &str, data: Option<&[u8]>) {
    hasher.update(&(tag.len() as u64).to_le_bytes());
    hasher.update(tag.as_bytes());
    match data {
        Some(data) => {
            hasher.update(&[1]);
            hasher.update(&(data.len() as u64).to_le_bytes());
            hasher.update(data);
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

/// The buffers a `.gltf` references by URI (its `.bin` files: vertex data, and so the collider
/// cooked from it), tagged by their path from the `.gltf`'s folder, with `None` for one that is
/// missing. Images are left out: they are presentation, not gameplay content (audit wave 6,
/// netplug M1: two trees with the same `.gltf` and different `.bin` hashed alike).
fn gltf_buffer_inputs(source: &Path) -> Vec<(String, Option<Vec<u8>>)> {
    const IMAGE_EXTENSIONS: [&str; 7] = ["png", "jpg", "jpeg", "ktx2", "webp", "dds", "basis"];
    let Some(dir) = source.parent() else {
        return Vec::new();
    };
    regia_asset_pipeline::gltf_sidecars(std::slice::from_ref(&source.to_path_buf()))
        .into_iter()
        .filter(|path| {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            !IMAGE_EXTENSIONS
                .iter()
                .any(|image| ext.eq_ignore_ascii_case(image))
        })
        .map(|path| {
            let tag = path
                .strip_prefix(dir)
                .unwrap_or(&path)
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            let data = std::fs::read(&path).ok();
            (format!("gltf-buffer:{tag}"), data)
        })
        .collect()
}

/// The hash of a cooked asset's source: the file and its `.meta` (import settings change the
/// cooked result), framed so the split between them counts. A heightfield cooked out of a terrain
/// layer file also hashes the height sample files beside it (audit wave 5, netplug H1: those
/// bytes are the ground, and the layer file alone does not name them). Cooked bytes differ per
/// platform, and a Linux server must match a Windows client. Falls back to the cooked file for an
/// asset with no source file of its own (a tiled terrain).
fn source_hash(project_dir: &Path, cooked_dir: &Path, asset: &ClosureAsset) -> Result<u64> {
    let source = normalize_ref(&asset.path);
    let file = project_dir.join("assets").join(&source);
    if !file.is_file() {
        let cooked = cooked_dir.join(cooked_file_name(asset.id));
        let bytes = std::fs::read(&cooked).with_context(|| format!("reading {}", cooked.display()))?;
        return Ok(hash64(&bytes));
    }
    let bytes = std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"regia-source-hash-v2\0");
    framed(&mut hasher, "source", Some(&bytes));
    let mut meta = file.clone().into_os_string();
    meta.push(".meta");
    framed(&mut hasher, "meta", std::fs::read(&meta).ok().as_deref());
    for (tag, data) in gltf_buffer_inputs(&file) {
        framed(&mut hasher, &tag, data.as_deref());
    }
    if cooked_kind(&asset.type_path) == "heightfield" {
        // The platform-neutral ground hash (ADR-2440): portable tags, canonical extents, explicit
        // little-endian framing, so an Android and a desktop build of the same content agree.
        let ground = regia_asset_pipeline::terrain_cook::terrain_ground_hash(&file)
            .with_context(|| format!("hashing the ground of {}", file.display()))?;
        framed(&mut hasher, "terrain-ground", Some(&ground));
    }
    let mut out = [0u8; 8];
    out.copy_from_slice(&hasher.finalize().as_bytes()[..8]);
    Ok(u64::from_le_bytes(out))
}

/// The hashed-kind assets of the whole project, as the manifest builder takes them.
///
/// # Errors
///
/// No cooked cache, or a hashed file cannot be read.
pub fn manifest_assets(project_dir: &Path, platform: &str) -> Result<Vec<ManifestAsset>> {
    let closure = build_profile::compute_closure(project_dir, platform, None)?;
    let uses = asset_use(project_dir, &closure)?;
    let cooked_dir = cooked_cache_dir(project_dir, platform);
    let mut out = Vec::new();
    for asset in &closure.assets {
        let kind = cooked_use_kind(asset, &uses);
        if !is_hashed_kind(kind) {
            continue;
        }
        out.push(ManifestAsset {
            id: asset.path.clone(),
            kind: kind.to_owned(),
            content_hash: source_hash(project_dir, &cooked_dir, asset)?,
        });
    }
    for raw in &closure.raw {
        let kind = raw_kind(&raw.path);
        if !is_hashed_kind(kind) {
            continue;
        }
        let file = project_dir.join("assets").join(&raw.path);
        let bytes = std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
        out.push(ManifestAsset {
            id: raw.path.clone(),
            kind: kind.to_owned(),
            content_hash: hash64(&bytes),
        });
    }
    Ok(out)
}

/// The text of `net-manifest.ron`. `protocol_hash` stays 0 in the file: it hashes the types the game
/// registers at runtime, so each process fills in its own at startup and the handshake compares
/// those (ADR-1886).
///
/// # Errors
///
/// A hashed file cannot be read.
pub fn net_manifest_text(project_dir: &Path, platform: &str, game_version: &str) -> Result<String> {
    let assets = manifest_assets(project_dir, platform)?;
    let manifest = ContentManifest::build(
        env!("CARGO_PKG_VERSION"),
        game_version,
        NET_PROTOCOL_VERSION,
        0,
        &[(BASE_PACK, assets.as_slice())],
    );
    manifest.to_ron().map_err(|e| anyhow::anyhow!("{e}"))
}

/// Writes `<content_dir>/net-manifest.ron` (tmp + rename).
///
/// # Errors
///
/// A hashed file cannot be read, or the manifest cannot be written.
pub fn write_net_manifest(
    project_dir: &Path,
    platform: &str,
    game_version: &str,
    content_dir: &Path,
) -> Result<PathBuf> {
    let text = net_manifest_text(project_dir, platform, game_version)?;
    std::fs::create_dir_all(content_dir)?;
    let path = content_dir.join("net-manifest.ron");
    let tmp = content_dir.join("net-manifest.ron.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// The written package's content against the filter (spec 76 § 3): fails when `root` holds a kind a
/// server must not ship, in `content/base.fpak` or under `assets/`. A mesh passes only as the
/// `collision` asset the filter kept.
///
/// # Errors
///
/// The pak cannot be read, or a presentation asset reached the package.
pub fn check_staged_content(content: &ServerContent, root: &Path) -> Result<()> {
    let pak = root.join("content").join("base.fpak");
    let reader =
        regia_asset::PakReader::open(&pak).map_err(|e| anyhow::anyhow!("reading {}: {e}", pak.display()))?;
    let mut bad: Vec<String> = reader
        .manifest()
        .iter()
        .filter(|e| {
            let kind = cooked_kind(&e.type_path);
            is_presentation_kind(kind) && content.kept_kinds.get(&e.path) != Some(&"collision")
        })
        .map(|e| format!("{} [{}]", e.path, cooked_kind(&e.type_path)))
        .collect();
    let assets = root.join("assets");
    if assets.is_dir() {
        for entry in walkdir::WalkDir::new(&assets).sort_by_file_name() {
            let entry = entry.with_context(|| format!("walking {}", assets.display()))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = entry
                .path()
                .strip_prefix(&assets)
                .context("asset path escaped the assets root")?
                .to_string_lossy()
                .replace('\\', "/");
            if is_presentation_kind(raw_kind(&rel)) {
                bad.push(format!("assets/{rel} [{}]", raw_kind(&rel)));
            }
        }
    }
    if !bad.is_empty() {
        bail!(
            "the server content holds kinds a server never ships:\n  {}",
            bad.join("\n  ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::AssetId;

    #[test]
    fn audit_server_filter_refuses_an_unreadable_level() {
        let dir = regia_test_utils::unique_temp_dir("server-unreadable-level");
        let closure = Closure {
            raw: vec![ClosureRaw {
                path: "missing.level.ron".into(),
                bytes: 0,
            }],
            ..Closure::default()
        };
        let error =
            filter_closure(&dir, "linux", &closure).expect_err("missing gameplay use must not mean unused");
        assert!(error.to_string().contains("missing.level.ron"), "{error}");
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn audit_raw_pose_roots_are_kept_but_unrelated_montages_are_filtered() {
        let dir = regia_test_utils::unique_temp_dir("server-raw-pose");
        write(
            &dir.join("assets/main.level.ron"),
            br#"SceneFile(format_version: 8, entities: [
            (components: {
                "my_game::HitShapes": (pose_source: Joints),
                "regia_animation::Animator": (graph: "pose.animgraph.ron"),
            }),
        ])"#,
        );
        write(&dir.join("assets/pose.animgraph.ron"), b"(format_version: 1)");
        write(&dir.join("assets/unrelated.montage.ron"), b"(format_version: 1)");
        let closure = Closure {
            raw: ["main.level.ron", "pose.animgraph.ron", "unrelated.montage.ron"]
                .into_iter()
                .map(|path| ClosureRaw {
                    path: path.into(),
                    bytes: 0,
                })
                .collect(),
            ..Closure::default()
        };
        let filtered = filter_closure(&dir, "linux", &closure).expect("filter");
        assert!(
            filtered
                .kept
                .raw
                .iter()
                .any(|raw| raw.path == "pose.animgraph.ron")
        );
        assert!(
            filtered
                .filtered_raw
                .iter()
                .any(|(raw, _)| raw.path == "unrelated.montage.ron")
        );
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    /// A split level's entity files are scanned like a one-file level: the pose graph one of
    /// them names is kept, not filtered as presentation.
    #[test]
    fn audit_split_level_entities_are_scanned_for_gameplay_uses() {
        let dir = regia_test_utils::unique_temp_dir("server-split-level-use");
        write(
            &dir.join("assets/main.level.d/level.ron"),
            b"(format_version: 1, scene_format_version: 8)",
        );
        write(
            &dir.join("assets/main.level.d/entities/00000000000000000000000000000001.entity.ron"),
            br#"(components: {
                "my_game::HitShapes": (pose_source: Joints),
                "regia_animation::Animator": (graph: "pose.animgraph.ron"),
            })"#,
        );
        write(&dir.join("assets/pose.animgraph.ron"), b"(format_version: 1)");
        let closure = Closure {
            raw: [
                "main.level.d/level.ron",
                "main.level.d/entities/00000000000000000000000000000001.entity.ron",
                "pose.animgraph.ron",
            ]
            .into_iter()
            .map(|path| ClosureRaw {
                path: path.into(),
                bytes: 0,
            })
            .collect(),
            ..Closure::default()
        };
        let filtered = filter_closure(&dir, "linux", &closure).expect("filter");
        assert!(
            filtered
                .kept
                .raw
                .iter()
                .any(|raw| raw.path == "pose.animgraph.ron"),
            "{:?}",
            filtered.filtered_raw
        );
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    fn asset(path: &str, type_path: &str) -> ClosureAsset {
        ClosureAsset {
            path: path.to_owned(),
            id: AssetId::new_random(),
            type_path: type_path.to_owned(),
            bytes: 0,
        }
    }

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(path, bytes).expect("write");
    }

    /// Audit wave 5 (netplug M1): a file that holds the bytes of a source and its `.meta` does not
    /// hash like that source and `.meta`.
    #[test]
    fn a_source_and_its_meta_do_not_hash_like_one_file_holding_both() {
        let one = regia_test_utils::unique_temp_dir("srv-hash-split-a");
        let two = regia_test_utils::unique_temp_dir("srv-hash-split-b");
        write(&one.join("assets/meshes/rock.glb"), b"MESH");
        write(&one.join("assets/meshes/rock.glb.meta"), b"(scale: 0.01)");
        write(&two.join("assets/meshes/rock.glb"), b"MESH(scale: 0.01)");
        let rock = asset("assets/meshes/rock.glb", "regia_asset::CookedMesh");
        let a = source_hash(&one, &one.join("cooked"), &rock).expect("hash");
        let b = source_hash(&two, &two.join("cooked"), &rock).expect("hash");
        assert_ne!(a, b);
        // The same tree hashes the same, and an empty `.meta` is not a missing one.
        assert_eq!(a, source_hash(&one, &one.join("cooked"), &rock).expect("hash"));
        write(&two.join("assets/meshes/rock.glb.meta"), b"");
        let c = source_hash(&two, &two.join("cooked"), &rock).expect("hash");
        assert_ne!(b, c);
        let _ = std::fs::remove_dir_all(&one);
        let _ = std::fs::remove_dir_all(&two);
    }

    /// Audit wave 6 (netplug M1): a `.gltf` hashes the `.bin` buffers it references, so two
    /// trees with the same `.gltf` and `.meta` and different vertex data (a different collider)
    /// do not agree; a missing buffer is not an empty one, and an image it references is not
    /// part of the gameplay hash.
    #[test]
    fn a_gltf_hash_covers_the_buffers_it_references() {
        let gltf = br#"{"asset":{"version":"2.0"},"buffers":[{"uri":"crate.bin","byteLength":4}],"images":[{"uri":"crate.png"}]}"#;
        let crate_mesh = asset("assets/props/crate.gltf", "regia_asset::CookedMesh");
        let hash_with = |bin: Option<&[u8]>, png: &[u8]| {
            let root = regia_test_utils::unique_temp_dir("srv-hash-gltf-buffer");
            write(&root.join("assets/props/crate.gltf"), gltf);
            write(&root.join("assets/props/crate.gltf.meta"), b"()");
            if let Some(bin) = bin {
                write(&root.join("assets/props/crate.bin"), bin);
            }
            write(&root.join("assets/props/crate.png"), png);
            let hash = source_hash(&root, &root.join("cooked"), &crate_mesh).expect("hash");
            let _ = std::fs::remove_dir_all(&root);
            hash
        };
        let collider = hash_with(Some(b"BOX!"), b"red");
        assert_eq!(
            collider,
            hash_with(Some(b"BOX!"), b"blue"),
            "a texture is not gameplay"
        );
        assert_ne!(collider, hash_with(Some(b"FLAT"), b"red"), "a different buffer");
        assert_ne!(
            hash_with(None, b"red"),
            hash_with(Some(b""), b"red"),
            "missing is not empty"
        );
    }

    /// Audit wave 5 (netplug H1): a heightfield cooked from a terrain layer file hashes the
    /// height samples beside it, so two packages with the same layer file and different ground
    /// do not agree. A texture beside it is not part of the gameplay hash.
    #[test]
    fn a_terrain_heightfield_hash_covers_its_height_samples() {
        let hashes: Vec<u64> = [b"mountain".as_slice(), b"flat".as_slice()]
            .into_iter()
            .map(|samples| {
                let root = regia_test_utils::unique_temp_dir("srv-hash-heightfield");
                write(
                    &root.join("assets/maps/island/island.terrain.layers.ron"),
                    b"(layers: [])",
                );
                write(&root.join("assets/maps/island/heightfield.png"), samples);
                let ground = asset(
                    "assets/maps/island/island.terrain.layers.ron#terrain/heightfield",
                    "regia_asset::Heightfield",
                );
                let before = source_hash(&root, &root.join("cooked"), &ground).expect("hash");
                write(&root.join("assets/maps/island/macro_color.png"), b"paint");
                let after = source_hash(&root, &root.join("cooked"), &ground).expect("hash");
                assert_eq!(before, after, "a presentation texture is not gameplay content");
                let _ = std::fs::remove_dir_all(&root);
                before
            })
            .collect();
        assert_ne!(hashes[0], hashes[1]);
    }

    #[test]
    fn cooked_types_map_to_kinds() {
        assert_eq!(cooked_kind("regia_asset::CookedTexture"), "texture");
        assert_eq!(cooked_kind("regia_asset::CookedStandardMaterial"), "material");
        assert_eq!(cooked_kind("regia_asset::CookedMesh"), "mesh");
        assert_eq!(cooked_kind("regia_animation::Skeleton"), "skeleton");
        assert_eq!(cooked_kind("regia_animation::AnimationClip"), "clip");
        assert_eq!(cooked_kind("regia_asset::Heightfield"), "heightfield");
        assert_eq!(cooked_kind("regia_audio::SoundLoudness"), "audio");
        assert_eq!(cooked_kind("my_game::CookedWaveTable"), "data");
    }

    #[test]
    fn raw_names_map_to_kinds() {
        for (path, kind) in [
            ("levels/a.level.ron", "level"),
            ("prefabs/b.prefab.ron", "prefab"),
            ("net/p.netprofile.ron", "netprofile"),
            ("data/weapons.table.ron", "data_table"),
            ("data/weapons.table.csv", "data_table"),
            ("config/net.ron", "config"),
            ("nav/arena.navmesh.ron", "navmesh"),
            ("data/tuning.ron", "data"),
            ("ui/hud.ui.ron", "ui"),
            ("fx/spark.vfx.ron", "vfx"),
            ("audio/gun.ogg", "audio"),
            ("audio/gun.cue.ron", "audio"),
            ("fonts/a.ttf", "font"),
            ("shaders/a.wgsl", "shader"),
            ("tex/a.PNG", "texture"),
            ("cine/intro.shots.ron", "cinematic"),
        ] {
            assert_eq!(raw_kind(path), kind, "{path}");
        }
    }

    #[test]
    fn every_presentation_kind_is_outside_the_hash() {
        for kind in PRESENTATION_KINDS.iter().chain(POSE_KINDS) {
            assert!(!is_hashed_kind(kind), "{kind} must not be hashed (spec 77 § 4.1)");
        }
        for kind in regia_net::session::manifest::HASHED_KINDS {
            assert!(!is_presentation_kind(kind), "{kind} is server content");
        }
    }

    const LEVEL: &str = r#"(
    format_version: 1,
    // a comment with "assets://ignored/mesh.glb"
    entities: [
        (name: "rock", components: {
            "regia_physics::components::Collider": (mesh: "assets://meshes/rock.glb#Mesh0"),
            "regia_render::Model": (model: "meshes/rock_visual.glb"),
        }),
        (name: "soldier", components: {
            "my_game::HitShapes": (pose_source: Joints, shapes: []),
            "regia_animation::Animator": (skeleton: "chars/soldier.glb#Skeleton", clip: "chars/run.glb"),
        }),
        (name: "boss", components: {
            "regia_framework::character::RootMotionDriven": (),
            "my_game::Rig": (asset: "chars/boss.glb"),
        }),
        (name: "dummy", components: {
            "my_game::HitShapes": (pose_source: Authored, shapes: []),
            "regia_animation::Animator": (skeleton: "chars/dummy.glb"),
        }),
    ],
)"#;

    #[test]
    fn level_use_is_read_per_entity() {
        let mut uses = AssetUse::default();
        scan_use(LEVEL, &mut uses);
        assert!(uses.collision.contains("meshes/rock.glb"), "{uses:?}");
        assert!(!uses.collision.contains("meshes/rock_visual.glb"), "{uses:?}");
        assert!(!uses.collision.contains("ignored/mesh.glb"), "{uses:?}");
        for root in ["chars/soldier.glb", "chars/run.glb", "chars/boss.glb"] {
            assert!(uses.pose_roots.contains(root), "{root}: {uses:?}");
        }
        assert!(!uses.pose_roots.contains("chars/dummy.glb"), "{uses:?}");
        assert!(!uses.pose_roots.contains("meshes/rock.glb"), "{uses:?}");
    }

    #[test]
    fn broken_text_gives_no_panic() {
        for text in [
            "",
            "(",
            "\"",
            "entities: [",
            "entities: [(",
            "(a: \"x",
            "/*",
            "é: \"ü\"",
        ] {
            let mut uses = AssetUse::default();
            scan_use(text, &mut uses);
        }
    }

    #[test]
    fn a_file_without_entities_is_one_scope() {
        let mut uses = AssetUse::default();
        scan_use(r#"(collider: (mesh: "a/b.glb"))"#, &mut uses);
        assert!(uses.collision.contains("a/b.glb"), "{uses:?}");
    }

    #[test]
    fn references_lose_their_prefix_and_sub_asset() {
        assert_eq!(normalize_ref("assets://a/b.glb#Mesh0"), "a/b.glb");
        assert_eq!(normalize_ref("assets/a/b.glb"), "a/b.glb");
        assert_eq!(normalize_ref("./a/b.glb"), "a/b.glb");
        assert_eq!(normalize_ref("a/b.glb"), "a/b.glb");
    }

    /// A throwaway project: a level that uses every asset kind, and files no level uses.
    struct Project(PathBuf);

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl Project {
        fn write(&self, rel: &str, bytes: &[u8]) {
            let path = self.0.join("assets").join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(path, bytes).expect("write");
        }

        fn cook(&self) {
            let report = regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions {
                project_root: self.0.clone(),
                platform: "linux".into(),
                incremental: true,
            })
            .expect("cook");
            crate::commands::cook::report_cook_outcome(&report).expect("cook outcome");
        }

        fn pack(&self) {
            let cooked = cooked_cache_dir(&self.0, "linux");
            regia_asset_pipeline::pack_cooked_directory(
                &cooked,
                &self.0.join("build/paks/linux"),
                &std::collections::BTreeMap::new(),
                true,
            )
            .expect("pack");
            // A finished pack, as `regia cook` and `regia package` record it (ADR-7413).
            crate::commands::package::write_pack_stamp(&self.0, &self.0.join("build/paks/linux"))
                .expect("pack stamp");
        }
    }

    /// A `.glb` from its JSON and binary chunks.
    fn glb(json: &str, mut bin: Vec<u8>) -> Vec<u8> {
        let mut json = json.as_bytes().to_vec();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        while !bin.len().is_multiple_of(4) {
            bin.push(0);
        }
        let total = 12 + 8 + json.len() + 8 + bin.len();
        let mut out = Vec::new();
        out.extend_from_slice(b"glTF");
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&(json.len() as u32).to_le_bytes());
        out.extend_from_slice(b"JSON");
        out.extend_from_slice(&json);
        out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        out.extend_from_slice(b"BIN\0");
        out.extend_from_slice(&bin);
        out
    }

    fn floats(values: &[f32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// A one-triangle `.glb`.
    fn tiny_glb() -> Vec<u8> {
        let json = r#"{"asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0]}],"nodes":[{"mesh":0}],"meshes":[{"primitives":[{"attributes":{"POSITION":0}}]}],"accessors":[{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]}],"bufferViews":[{"buffer":0,"byteLength":36}],"buffers":[{"byteLength":36}]}"#;
        glb(json, floats(&[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]))
    }

    /// A `.glb` with one joint and one clip: positions 0..36, times 36..44, translations 44..68.
    fn rigged_glb() -> Vec<u8> {
        let json = r#"{"asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0,1]}],"nodes":[{"name":"Tri","mesh":0},{"name":"Hips"}],"meshes":[{"primitives":[{"attributes":{"POSITION":0}}]}],"skins":[{"joints":[1]}],"animations":[{"name":"walk","samplers":[{"input":1,"output":2}],"channels":[{"sampler":0,"target":{"node":1,"path":"translation"}}]}],"accessors":[{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]},{"bufferView":1,"componentType":5126,"count":2,"type":"SCALAR","min":[0],"max":[1]},{"bufferView":2,"componentType":5126,"count":2,"type":"VEC3"}],"bufferViews":[{"buffer":0,"byteOffset":0,"byteLength":36},{"buffer":0,"byteOffset":36,"byteLength":8},{"buffer":0,"byteOffset":44,"byteLength":24}],"buffers":[{"byteLength":68}]}"#;
        let mut bin = floats(&[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
        bin.extend(floats(&[0.0, 1.0]));
        bin.extend(floats(&[0.0, 0.0, 0.0, 1.0, 0.0, 0.0]));
        glb(json, bin)
    }

    fn tiny_png() -> Vec<u8> {
        let mut bytes = Vec::new();
        let image = image::RgbaImage::from_pixel(4, 4, image::Rgba([200, 100, 50, 255]));
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .expect("png");
        bytes
    }

    /// A short Ogg Vorbis tone (`regia_audio`'s test fixture).
    const GUN_OGG: &[u8] = include_bytes!("../../../regia_audio/tests/fixtures/tone_1k.ogg");

    fn project(label: &str) -> Project {
        let p = Project(regia_test_utils::unique_temp_dir(label));
        std::fs::create_dir_all(&p.0).expect("dir");
        std::fs::write(
            p.0.join("Regia.toml"),
            "[project]\nname = \"Fix\"\nid = \"dev.regia.fix\"\nversion = \"1.2.3\"\n",
        )
        .expect("Regia.toml");
        p.write(
            "levels/main.level.ron",
            br#"SceneFile(format_version: 8, entities: [
                (name: "rock", components: {
                    "regia_physics::Collider": (mesh: "assets://meshes/rock.glb"),
                    "regia_render::MeshRenderer": (mesh: "meshes/visual.glb", texture: "textures/ground.png"),
                }),
            ])"#,
        );
        p.write(
            "prefabs/crate.prefab.ron",
            b"SceneFile(format_version: 8, entities: [])",
        );
        p.write("net/character.netprofile.ron", b"(format_version: 1)");
        p.write("data/weapons.table.ron", b"(format_version: 1, rows: {})");
        p.write("data/tuning.ron", b"(speed: 3.0)");
        p.write("config/net.ron", b"(format_version: 1)");
        p.write("meshes/rock.glb", &tiny_glb());
        p.write("meshes/visual.glb", &tiny_glb());
        p.write("textures/ground.png", &tiny_png());
        // A real Ogg Vorbis file: the cook decodes sounds to measure loudness (spec 95).
        p.write("audio/gun.ogg", GUN_OGG);
        p.write("audio/gun.cue.ron", b"(variations: [])");
        p.write("fonts/body.ttf", b"not really a font");
        // A loadable layout: the shared root resolver (ADR-8477) refuses a layout the runtime
        // cannot load (`content_root_malformed`), as this one was without a `version`.
        p.write("ui/hud.ui.ron", b"(version: 9, root: ())");
        p.write("fx/spark.vfx.ron", b"(format_version: 1)");
        p.write("shaders/water.wgsl", b"// nothing");
        p.cook();
        p.pack();
        p
    }

    fn server_profile() -> ResolvedProfile {
        ResolvedProfile {
            name: "server".into(),
            cargo: crate::commands::package::Profile::Dev,
            spec: None,
            server: true,
        }
    }

    fn stage_server(p: &Project, root: &Path) -> ServerContent {
        build_profile::stage_server_content(&p.0, "linux", &server_profile(), root, "1.2.3").expect("stage")
    }

    fn pak_types(pak: &Path) -> Vec<(String, String)> {
        let reader = regia_asset::PakReader::open(pak).expect("pak");
        reader
            .manifest()
            .iter()
            .map(|e| (e.path.clone(), e.type_path.clone()))
            .collect()
    }

    /// Spec 76 § 11 test 3.
    #[test]
    fn the_server_pak_holds_gameplay_content_only() {
        let p = project("server-filter");
        let root = p.0.join("dist/server");
        let content = stage_server(&p, &root);
        for raw in [
            "levels/main.level.ron",
            "prefabs/crate.prefab.ron",
            "net/character.netprofile.ron",
            "data/weapons.table.ron",
            "data/tuning.ron",
            "config/net.ron",
        ] {
            assert!(root.join("assets").join(raw).is_file(), "{raw} is server content");
        }
        for raw in [
            "audio/gun.ogg",
            "audio/gun.cue.ron",
            "fonts/body.ttf",
            "ui/hud.ui.ron",
            "fx/spark.vfx.ron",
            "shaders/water.wgsl",
        ] {
            assert!(!root.join("assets").join(raw).exists(), "{raw} stays out");
        }
        let entries = pak_types(&root.join("content/base.fpak"));
        assert!(
            entries
                .iter()
                .any(|(path, ty)| path.contains("rock.glb") && ty == "regia_asset::CookedMesh"),
            "the mesh a collider names is kept: {entries:?}"
        );
        assert!(
            entries.iter().all(|(path, _)| !path.contains("visual.glb")),
            "a render mesh stays out: {entries:?}"
        );
        for (path, ty) in &entries {
            assert_ne!(cooked_kind(ty), "texture", "{path}");
            assert_ne!(cooked_kind(ty), "material", "{path}");
            assert_ne!(cooked_kind(ty), "model", "{path}");
        }
        assert!(
            content
                .filtered_assets
                .iter()
                .any(|(a, kind)| *kind == "texture" && a.path.contains("ground")),
            "the report names what it removed"
        );
        assert!(root.join("content/net-manifest.ron").is_file());
    }

    /// Spec 76 § 11 test 4.
    #[test]
    fn client_and_server_packages_write_the_same_manifest() {
        let p = project("server-parity");
        let server = p.0.join("dist/server");
        stage_server(&p, &server);
        let client = p.0.join("dist/client");
        let plain = ResolvedProfile {
            name: "dev".into(),
            cargo: crate::commands::package::Profile::Dev,
            spec: None,
            server: false,
        };
        build_profile::stage_content(
            &p.0,
            "linux",
            &plain,
            &client,
            &client.join("content"),
            &client.join("assets"),
            || Ok(0),
        )
        .expect("client");
        let a = std::fs::read(server.join("content/net-manifest.ron")).expect("server manifest");
        let b = std::fs::read(client.join("content/net-manifest.ron")).expect("client manifest");
        assert_eq!(a, b, "byte-identical");
        let manifest = ContentManifest::from_ron(&String::from_utf8_lossy(&a)).expect("parses");
        assert_eq!(manifest.game_version, "1.2.3");
        assert_eq!(manifest.packs.len(), 1);
        assert_eq!(manifest.protocol_hash, 0, "each process fills in its own");
    }

    #[test]
    fn gameplay_changes_move_the_hash_and_presentation_changes_do_not() {
        let p = project("server-hash");
        let hash = |p: &Project| {
            p.cook();
            net_manifest_text(&p.0, "linux", "1").expect("manifest")
        };
        let base = hash(&p);
        p.write("ui/hud.ui.ron", b"(root: (changed: true))");
        // Other bytes, still a decodable sound: padding after the last Ogg page.
        p.write("audio/gun.ogg", &[GUN_OGG, &[0u8; 16]].concat());
        p.write("textures/ground.png", &{
            let mut png = tiny_png();
            png.extend_from_slice(b"\0");
            png
        });
        assert_eq!(hash(&p), base, "presentation is not hashed");
        p.write("data/tuning.ron", b"(speed: 4.0)");
        assert_ne!(hash(&p), base, "gameplay data is");
    }

    #[test]
    fn a_collision_mesh_is_hashed_and_a_render_mesh_is_not() {
        let p = project("server-mesh");
        let assets = manifest_assets(&p.0, "linux").expect("assets");
        assert!(
            assets
                .iter()
                .any(|a| a.kind == "collision" && a.id.contains("rock.glb")),
            "{assets:?}"
        );
        assert!(!assets.iter().any(|a| a.id.contains("visual.glb")), "{assets:?}");
    }

    #[test]
    fn the_dry_run_lists_both_sides_and_stages_nothing() {
        let p = project("server-dry");
        let out = p.0.join("dist/server-linux");
        let args = crate::commands::package::PackageArgs {
            platform: crate::commands::package::Platform::Linux,
            format: None,
            profile_name: "server".into(),
            profile: crate::commands::package::Profile::Shipping,
            dry_run: true,
            project: p.0.clone(),
            out: Some(out.clone()),
            steam: false,
            abis: Vec::new(),
            glibc: Some("host".into()),
            pipeline_caches: None,
            pipeline_cache_references: Vec::new(),
            prepare_scenes: Vec::new(),
            server: true,
            json: false,
            audit_dir: None,
            audit_artifact: None,
            verify_receipt: None,
            container: None,
            container_image: None,
            container_mounts: Vec::new(),
            no_engine_credit: false,
        };
        let report = build_profile::dry_run(&p.0, &server_profile(), &args).expect("report");
        assert!(report.contains("levels/main.level.ron"), "{report}");
        assert!(report.contains("audio/gun.ogg"), "{report}");
        assert!(report.contains("[texture]"), "{report}");
        crate::commands::package::run(&args).expect("dry run");
        assert!(!out.exists(), "a dry run writes no package");
    }

    /// Spec 76 § 3: skeletons and clips stay for hit shapes on joints, and only for them.
    #[test]
    fn poses_keep_skeletons_and_clips_for_the_entities_that_need_them() {
        let kinds_in = |pose_source: &str, label: &str| {
            let p = project(label);
            p.write("chars/rig.glb", &rigged_glb());
            p.write(
                "levels/main.level.ron",
                format!(
                    r#"SceneFile(format_version: 8, entities: [
                        (name: "soldier", components: {{
                            "my_game::HitShapes": (pose_source: {pose_source}, shapes: []),
                            "regia_animation::Animator": (skeleton: "chars/rig.glb#skeleton/Hips", clip: "chars/rig.glb#clip/walk"),
                        }}),
                    ])"#
                )
                .as_bytes(),
            );
            p.cook();
            p.pack();
            let root = p.0.join("dist/server");
            stage_server(&p, &root);
            let kinds: BTreeSet<&'static str> = pak_types(&root.join("content/base.fpak"))
                .iter()
                .map(|(_, ty)| cooked_kind(ty))
                .collect();
            kinds
        };
        let joints = kinds_in("Joints", "server-pose");
        assert!(
            joints.contains("skeleton") && joints.contains("clip"),
            "{joints:?}"
        );
        let authored = kinds_in("Authored", "server-nopose");
        assert!(
            !authored.contains("skeleton") && !authored.contains("clip"),
            "{authored:?}"
        );
    }

    #[test]
    fn a_presentation_file_in_the_package_fails_the_check() {
        let p = project("server-check");
        let root = p.0.join("dist/server");
        let content = stage_server(&p, &root);
        check_staged_content(&content, &root).expect("what the filter kept passes");
        std::fs::create_dir_all(root.join("assets/tex")).expect("dir");
        std::fs::write(root.join("assets/tex/a.png"), b"x").expect("write");
        let err = check_staged_content(&content, &root).expect_err("refused");
        assert!(err.to_string().contains("tex/a.png"), "{err}");
    }

    #[test]
    fn a_client_profile_does_not_change_the_manifest() {
        let p = project("server-profile");
        let whole = net_manifest_text(&p.0, "linux", "1").expect("manifest");
        let root = p.0.join("dist/client");
        let demo = ResolvedProfile {
            name: "demo".into(),
            cargo: crate::commands::package::Profile::Dev,
            spec: Some(BuildProfile {
                entry_levels: vec!["levels/main.level.ron".into()],
                ..BuildProfile::default()
            }),
            server: false,
        };
        build_profile::stage_content(
            &p.0,
            "linux",
            &demo,
            &root,
            &root.join("content"),
            &root.join("assets"),
            || Ok(0),
        )
        .expect("client");
        let written = std::fs::read_to_string(root.join("content/net-manifest.ron")).expect("manifest");
        assert_eq!(
            ContentManifest::from_ron(&written).expect("parses").packs,
            ContentManifest::from_ron(&whole).expect("parses").packs,
            "data outside the profile's closure still counts"
        );
    }

    #[test]
    fn an_asset_id_reference_resolves_to_its_path() {
        let p = project("server-asset-id");
        let closure = build_profile::compute_closure(&p.0, "linux", None).expect("closure");
        let rock = closure
            .assets
            .iter()
            .find(|a| a.path.contains("visual.glb") && a.type_path == "regia_asset::CookedMesh")
            .expect("a mesh");
        p.write(
            "levels/main.level.ron",
            format!(
                r#"SceneFile(format_version: 8, entities: [
                    (name: "rock", components: {{ "regia_physics::Collider": (mesh: "asset:{}") }}),
                ])"#,
                rock.id.to_string().to_uppercase()
            )
            .as_bytes(),
        );
        let uses = asset_use(&p.0, &closure).expect("read gameplay uses");
        assert!(uses.collision.contains("meshes/visual.glb"), "{uses:?}");
    }
}
