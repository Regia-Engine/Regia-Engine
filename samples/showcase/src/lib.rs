// SPDX-License-Identifier: MIT-0
//! Showcase: a host for filming engine footage from a scene directory.
//!
//! The sample combines parts the engine already has and adds no engine feature:
//!
//! - a 16-bit heightfield drawn by the shared terrain pass, with splat layers
//!   (`.terrain.layers.ron`, optional per-layer material textures);
//! - rocks and vegetation placed from a `.scatter.ron` set;
//! - the sky and weather from a state file ([`sky::SkyFile`]);
//! - one named entity (a stand-in capsule) that follows a keyframed [`track::Track`];
//! - a [`spawns::SpawnFile`] that fires VFX and decals at given frames;
//! - optional engine features, each off unless its file or flag is present ([`features`]): terrain
//!   grass, valley mist, a stream's water ribbon, an entry streak, scatter contact AO and armed tunnels
//!   ([`tunnels`], ADR-8825);
//! - an opt-in walk through the scene in a headset ([`xr`], `--xr`).
//!
//! Everything is a function of the frame number of a fixed 60 Hz step. The agent protocol drives
//! the camera and the output (`camera.set_path`, `render.out_start`, `seeing.screenshot`).
//!
//! The CPU model (scene loading, track interpolation, spawn schedule) has unit tests that need no
//! GPU; `tests/render.rs` renders eight frames twice on lavapipe and compares hashes. The agent
//! guide page is `book/agents/showcase.md`.

pub mod app;
pub mod camera_physics;
pub mod cli;
pub mod demo;
pub mod example;
pub mod features;
pub mod gltf;
pub mod perf_table;
pub mod scene;
pub mod shake;
pub mod sky;
pub mod spawns;
pub mod track;
pub mod tunnels;
/// The armed tunnels' agent methods (`tunnels.*`).
#[cfg(feature = "agent")]
pub mod tunnels_agent;
pub mod tunnels_example;
pub mod xr;

use std::path::{Path, PathBuf};

/// Fixed step rate of the sample, in hertz.
pub const STEP_HZ: f64 = 60.0;

/// A stable, machine-readable reason a scene could not be loaded. The text form
/// ([`ErrorCode::as_str`]) is part of the sample's contract: lowercase ASCII, digits and `_`, and
/// listed in `book/agents/showcase.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// The scene path does not exist or is not a directory.
    SceneDirMissing,
    /// The directory has no `scene.ron`.
    SceneManifestMissing,
    /// `scene.ron` does not parse, has the wrong version or holds a value out of range.
    SceneManifestMalformed,
    /// A file the manifest names does not exist.
    SceneFileMissing,
    /// The heightfield is not a readable 16-bit grayscale PNG of a usable size.
    HeightfieldMalformed,
    /// The terrain layers file does not parse or fails validation.
    LayersMalformed,
    /// A layer material texture is unreadable or has the wrong shape.
    MaterialsMalformed,
    /// The `.scatter.ron` does not parse or fails validation.
    ScatterMalformed,
    /// The sky and weather file does not parse or holds a value out of range.
    SkyMalformed,
    /// The track file does not parse or holds a bad key.
    TrackMalformed,
    /// The spawn file does not parse or holds a bad entry.
    SpawnsMalformed,
    /// A spawn names a VFX effect that is neither built in nor in the scene's `vfx` directory.
    SpawnUnknownEffect,
    /// A spawn names a decal prefab that is neither built in nor in the scene's `vfx` directory.
    SpawnUnknownDecal,
    /// A spawn follows the tracked entity but the scene has no track.
    SpawnNeedsTrack,
    /// A splat pack is unreadable, too large, or the two packs differ in size.
    SplatMalformed,
    /// A scatter mesh path names a `.gltf` or `.glb` file that does not exist.
    ScatterMeshMissing,
    /// A scatter mesh file is not a glTF the sample can read.
    ScatterMeshMalformed,
    /// `terrain/grass.ron` does not parse, names a layer the terrain lacks or fails the grass bake.
    GrassMalformed,
    /// `terrain/stream.json` does not parse or describes a ribbon the water draw refuses.
    WaterMalformed,
    /// `entry.ron` does not parse or holds a bad track or knob.
    EntryMalformed,
    /// A mesh the track names (`mesh` or `swap_mesh`) is not a file in the scene directory.
    TrackMeshMissing,
    /// A mesh the track names is not a glTF the sample can read, or holds no triangles.
    TrackMeshMalformed,
    /// `tunnels/tunnels.ron`, its mesh-layers document or a passages file does not parse or breaks a rule.
    TunnelsMalformed,
    /// The heightfield is not the square ground lattice `tunnels.ron` describes (a sample off its counts).
    TunnelsGroundMismatch,
    /// The mesh-layers cook refused the tunnels (its own code is in the message).
    TunnelsCookFailed,
}

impl ErrorCode {
    /// Every code, in the order the book page lists them.
    pub const ALL: [ErrorCode; 25] = [
        ErrorCode::SceneDirMissing,
        ErrorCode::SceneManifestMissing,
        ErrorCode::SceneManifestMalformed,
        ErrorCode::SceneFileMissing,
        ErrorCode::HeightfieldMalformed,
        ErrorCode::LayersMalformed,
        ErrorCode::MaterialsMalformed,
        ErrorCode::ScatterMalformed,
        ErrorCode::SkyMalformed,
        ErrorCode::TrackMalformed,
        ErrorCode::SpawnsMalformed,
        ErrorCode::SpawnUnknownEffect,
        ErrorCode::SpawnUnknownDecal,
        ErrorCode::SpawnNeedsTrack,
        ErrorCode::SplatMalformed,
        ErrorCode::ScatterMeshMissing,
        ErrorCode::ScatterMeshMalformed,
        ErrorCode::GrassMalformed,
        ErrorCode::WaterMalformed,
        ErrorCode::EntryMalformed,
        ErrorCode::TrackMeshMissing,
        ErrorCode::TrackMeshMalformed,
        ErrorCode::TunnelsMalformed,
        ErrorCode::TunnelsGroundMismatch,
        ErrorCode::TunnelsCookFailed,
    ];

    /// The code as text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::SceneDirMissing => "scene_dir_missing",
            ErrorCode::SceneManifestMissing => "scene_manifest_missing",
            ErrorCode::SceneManifestMalformed => "scene_manifest_malformed",
            ErrorCode::SceneFileMissing => "scene_file_missing",
            ErrorCode::HeightfieldMalformed => "heightfield_malformed",
            ErrorCode::LayersMalformed => "layers_malformed",
            ErrorCode::MaterialsMalformed => "materials_malformed",
            ErrorCode::ScatterMalformed => "scatter_malformed",
            ErrorCode::SkyMalformed => "sky_malformed",
            ErrorCode::TrackMalformed => "track_malformed",
            ErrorCode::SpawnsMalformed => "spawns_malformed",
            ErrorCode::SpawnUnknownEffect => "spawn_unknown_effect",
            ErrorCode::SpawnUnknownDecal => "spawn_unknown_decal",
            ErrorCode::SpawnNeedsTrack => "spawn_needs_track",
            ErrorCode::SplatMalformed => "splat_malformed",
            ErrorCode::ScatterMeshMissing => "scatter_mesh_missing",
            ErrorCode::ScatterMeshMalformed => "scatter_mesh_malformed",
            ErrorCode::GrassMalformed => "grass_malformed",
            ErrorCode::WaterMalformed => "water_malformed",
            ErrorCode::EntryMalformed => "entry_malformed",
            ErrorCode::TrackMeshMissing => "track_mesh_missing",
            ErrorCode::TrackMeshMalformed => "track_mesh_malformed",
            ErrorCode::TunnelsMalformed => "tunnels_malformed",
            ErrorCode::TunnelsGroundMismatch => "tunnels_ground_mismatch",
            ErrorCode::TunnelsCookFailed => "tunnels_cook_failed",
        }
    }
}

/// A scene that could not be loaded: a stable [`ErrorCode`], the file at fault and a message.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("{}: {}: {}", .code.as_str(), .path.display(), .message)]
pub struct ShowcaseError {
    /// Why.
    pub code: ErrorCode,
    /// The file or directory at fault.
    pub path: PathBuf,
    /// A human-readable detail.
    pub message: String,
}

impl ShowcaseError {
    /// An error of `code` at `path`.
    pub fn new(code: ErrorCode, path: &Path, message: impl Into<String>) -> Self {
        Self {
            code,
            path: path.to_path_buf(),
            message: message.into(),
        }
    }
}

/// Writes `bytes` to `path` through a sibling temp file and a rename.
///
/// # Errors
/// The IO error as text.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}
