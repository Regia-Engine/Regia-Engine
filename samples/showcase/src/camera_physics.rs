// SPDX-License-Identifier: MIT-0
//! The camera as a physical actor and the startled bird in the showcase (ADR-1502,
//! `book/agents/camera-body.md`).
//!
//! The host installs `regia_framework`'s [`CameraBodyPlugin`] and [`BirdPlugin`] and connects
//! them to its world:
//!
//! - **Ground.** The scene's heightfield is the spray's ground and the bird's floor
//!   ([`SprayGround`]).
//! - **Grass.** The body's wake goes to the grass every frame ([`TerrainGrassHandle::set_wake`]),
//!   so blades flick as the front passes.
//! - **Scene files.** Two optional files in the scene directory:
//!   - `camera_body.json` is a [`CameraBodySpec`]. Without it the body is installed but off, so
//!     scenes that predate it render exactly as before; `camera_body.set {"enabled": true}`
//!     switches it on.
//!   - `birds.json` is `{ "version": 1, "birds": [BirdSpec, ...] }`. The birds are queued at
//!     start-up; `bird.spawn` adds more.
//!
//! A bad file is a warning, not a failure: the scene still renders, without that part.

use std::path::Path;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_framework::bird::{BirdFlock, BirdPlugin, BirdSpec};
use regia_framework::camera_body::{
    CameraBody, CameraBodyPlugin, CameraBodySet, CameraBodySpec, SprayGround,
};
use regia_terrain::TerrainGrassHandle;
use regia_terrain::grass::GrassWake;
use serde::Deserialize;

use crate::scene::SceneData;

/// The camera-body spec file in a scene directory.
pub const CAMERA_BODY_FILE: &str = "camera_body.json";
/// The birds file in a scene directory.
pub const BIRDS_FILE: &str = "birds.json";
/// Format version of [`BIRDS_FILE`].
pub const BIRDS_FILE_VERSION: u32 = 1;

/// `birds.json`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BirdsFile {
    /// Format version ([`BIRDS_FILE_VERSION`]).
    pub version: u32,
    /// The birds (each checked like `bird.spawn`'s spec).
    pub birds: Vec<serde_json::Value>,
}

/// The camera-body spec of a scene: `camera_body.json` when present (enabled unless it says
/// otherwise), else the default spec switched off.
#[must_use]
pub fn scene_camera_body(dir: &Path) -> CameraBodySpec {
    let path = dir.join(CAMERA_BODY_FILE);
    let off = CameraBodySpec {
        enabled: false,
        ..CameraBodySpec::default()
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return off;
    };
    match CameraBodySpec::from_json(&text) {
        Ok(spec) => spec,
        Err(err) => {
            tracing::warn!(target: "showcase", "{}: {err}; the camera body stays off", path.display());
            off
        }
    }
}

/// The birds of a scene (`birds.json`), each checked; bad entries are skipped with a warning.
#[must_use]
pub fn scene_birds(dir: &Path) -> Vec<BirdSpec> {
    let path = dir.join(BIRDS_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let file: BirdsFile = match serde_json::from_str(&text) {
        Ok(file) => file,
        Err(err) => {
            tracing::warn!(target: "showcase", "{}: {err}; no birds", path.display());
            return Vec::new();
        }
    };
    if file.version > BIRDS_FILE_VERSION {
        tracing::warn!(target: "showcase", "{}: version {} is newer than {BIRDS_FILE_VERSION}; no birds", path.display(), file.version);
        return Vec::new();
    }
    let mut birds: Vec<BirdSpec> = Vec::new();
    for (i, value) in file.birds.iter().enumerate() {
        match BirdSpec::from_json(&value.to_string()) {
            Ok(spec) if birds.iter().any(|b| b.name == spec.name) => {
                tracing::warn!(target: "showcase", "{}: bird {i} repeats the name `{}`; skipped", path.display(), spec.name);
            }
            Ok(spec) => birds.push(spec),
            Err(err) => tracing::warn!(target: "showcase", "{}: bird {i}: {err}; skipped", path.display()),
        }
    }
    birds
}

/// Hands the body's wake to the grass each frame (`None` while the body makes none).
fn wake_to_grass(body: Res<CameraBody>, grass: Option<Res<TerrainGrassHandle>>) {
    let Some(grass) = grass else { return };
    let shape = body.spec().wake;
    let wake = body
        .state()
        .zip(body.previous())
        .map(|(now, before)| GrassWake::new(&shape, &now, &before))
        .filter(GrassWake::is_active);
    grass.set_wake(wake);
}

/// Installs the camera body and the birds for a scene (see the module docs).
pub struct CameraPhysicsPlugin {
    /// The loaded scene.
    pub scene: Arc<SceneData>,
}

impl Plugin for CameraPhysicsPlugin {
    fn build(&self, app: &mut App) {
        let scene = Arc::clone(&self.scene);
        app.insert_resource(CameraBody::new(scene_camera_body(&scene.dir)));
        let ground = Arc::clone(&scene);
        app.insert_resource(SprayGround {
            height: Some(Arc::new(move |x: f32, z: f32| {
                let half = 0.5 * ground.map.world_width();
                (x.abs() <= half && z.abs() <= half).then(|| ground.map.sample_bilinear(x, z))
            })),
            surface: None,
        });
        app.add_plugins((CameraBodyPlugin, BirdPlugin));
        let mut flock = BirdFlock::default();
        for spec in scene_birds(&scene.dir) {
            flock.spawn(spec);
        }
        app.insert_resource(flock);
        app.add_systems(PostUpdate, wake_to_grass.after(CameraBodySet));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_files_are_optional_and_checked() {
        let dir = regia_test_utils::unique_temp_dir("showcase-camera-physics");
        assert!(!scene_camera_body(&dir).enabled, "no file: installed but off");
        assert!(scene_birds(&dir).is_empty());
        let write = |name: &str, text: &str| {
            let tmp = dir.join(format!("{name}.tmp"));
            std::fs::write(&tmp, text).expect("write");
            std::fs::rename(&tmp, dir.join(name)).expect("rename");
        };
        write(CAMERA_BODY_FILE, r#"{ "wake": { "gain": 0.2 } }"#);
        let spec = scene_camera_body(&dir);
        assert!(spec.enabled);
        assert_eq!(spec.wake.gain, 0.2);
        write(
            BIRDS_FILE,
            r#"{ "version": 1, "birds": [
                { "name": "crow", "position": [0, 10, 0] },
                { "name": "crow", "position": [1, 10, 0] },
                { "name": "", "position": [0, 0, 0] },
                { "name": "gull", "species": "gull", "position": [5, 10, 0] }
            ] }"#,
        );
        let birds = scene_birds(&dir);
        let names: Vec<&str> = birds.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["crow", "gull"], "repeats and bad entries are skipped");
        write(CAMERA_BODY_FILE, "{ not json");
        assert!(!scene_camera_body(&dir).enabled, "a bad file leaves it off");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
