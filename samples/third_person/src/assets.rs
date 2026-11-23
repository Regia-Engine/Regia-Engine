// SPDX-License-Identifier: MIT-0
//! Fox asset plumbing: fetch the CC0 Khronos glTF, cook it through the CLI at startup, and
//! load skeleton + clips + skinned mesh from the cooked cache.
//!
//! The cooked mesh carries the authored `JOINTS_0` / `WEIGHTS_0` skin (cooked mesh format v2:
//! joint indices are cooked skeleton ids, weights `unorm16`), so the render mesh comes straight
//! from [`CpuMesh::from_cooked`]. GPU skinning then deforms it against the `SkinPalette` that
//! `regia_framework::skinning::SkinnedMeshPlugin` hands the renderer each frame. The `gltf`
//! crate stays a dev-dependency (the tests compare the cooked skin with the source): shipped
//! games may not link it (`check-layers`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_animation::Skeleton;
use regia_animation::clip::AnimationClip;
use regia_animation::cooked::{load_cooked_clip, load_cooked_skeleton};
use regia_app::prelude::*;
use regia_asset::{AssetManifest, load_mesh_bytes};
use regia_render::CpuMesh;

/// Where the Fox comes from.
pub const FOX_URL: &str =
    "https://raw.githubusercontent.com/KhronosGroup/glTF-Sample-Assets/main/Models/Fox/glTF-Binary/Fox.glb";

/// Everything the sample needs from the cooked Fox.
#[derive(Resource)]
pub struct FoxAssets {
    /// The cooked skeleton.
    pub skeleton: Arc<Skeleton>,
    /// The cooked clips (`Walk`, `Run`, `Survey`).
    pub clips: Vec<AnimationClip>,
    /// The skinned render mesh.
    pub mesh: Arc<CpuMesh>,
    /// Metres to drop the rig below the capsule centre so the lowest bind-pose
    /// vertex stands on the ground. See [`crate::character::rig_drop`].
    pub rig_drop: f32,
}

/// The sample project directory (this crate's).
pub fn project_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Serialises the one-time download and the in-place `regia cook` subprocess.
///
/// `authored_weights_cover_the_cooked_fox` and `cooked_skin_matches_the_gltf_skin` both call
/// [`ensure_cooked`], and Rust runs them on separate threads of the same test binary. Two
/// concurrent `regia cook --project samples/third_person` processes write the same
/// `.regia/` tree and `assets/*.meta` files, which raced to
/// "database error: io error: No such file or directory" under load. One lock makes the
/// cook (and the Fox download) exclusive per process; [`ensure_cooked`] adds a file lock for
/// the other test binaries of this package (the lib, `gameplay`, `agent_expect`), which a
/// parallel test runner such as the farm gate starts at the same time.
static COOK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Makes sure `assets/Fox.glb` exists (downloads once) and the cooked cache is fresh.
/// Desktop-only: Android builds have no importer (`anim_lab` has the same split).
///
/// Exclusive across threads (`COOK_LOCK`) and across processes (an advisory lock on
/// `.regia/third_person-cook.lock`, released when the guard drops).
pub fn ensure_cooked() -> Result<(), String> {
    let _guard = COOK_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = project_dir();
    let lock_path = dir.join(".regia").join("third_person-cook.lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("open {}: {e}", lock_path.display()))?;
    lock.lock()
        .map_err(|e| format!("lock {}: {e}", lock_path.display()))?;
    let glb = dir.join("assets/Fox.glb");
    ensure_glb(&glb, FOX_URL)?;
    cook(&dir)
}

/// Downloads `url` to `path` unless it is already there.
fn ensure_glb(path: &Path, url: &str) -> Result<(), String> {
    if let Ok(meta) = std::fs::metadata(path)
        && meta.len() > 1000
    {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    // A local Khronos sample set (the test-asset cache) avoids the network.
    if let (Ok(dir), Some(name)) = (std::env::var("REGIA_KHRONOS_ASSETS"), path.file_name()) {
        let local = Path::new(&dir).join(name);
        if local.is_file() {
            std::fs::copy(&local, path)
                .map_err(|e| format!("copy {} to {}: {e}", local.display(), path.display()))?;
            return Ok(());
        }
    }
    let status = std::process::Command::new("curl")
        .args(["-fsSL", "--retry", "3", "-o"])
        .arg(path)
        .arg(url)
        .status()
        .map_err(|e| format!("curl {url}: {e}"))?;
    if !status.success() {
        return Err(format!("curl exited {status} for {url}"));
    }
    Ok(())
}

/// Runs `regia cook` for the sample project (a no-op when the cache is fresh).
///
/// From a test binary the build goes to its own target dir ([`cook_target_dir`]): a
/// `cargo run -p regia_cli` into the `cargo test` run's own target dir resolves features for
/// `regia_cli` alone and rebuilds crates the run already built. `regia_web` (a `cdylib` path
/// crate, whose `libregia_web.rlib` cargo names without a hash) is then overwritten under the
/// running `cargo test`, and every doctest it compiles afterwards fails with E0460/E0463
/// (ADR-7484, the M5 run).
fn cook(project: &Path) -> Result<(), String> {
    let mut command = std::process::Command::new("cargo");
    command.args(["run", "-q", "-p", "regia_cli"]);
    if let Some(target) = std::env::current_exe().ok().and_then(|exe| cook_target_dir(&exe)) {
        command.arg("--target-dir").arg(target);
    }
    let status = command
        .args(["--", "cook", "--project"])
        .arg(project)
        .status()
        .map_err(|e| format!("cook subprocess: {e}"))?;
    if !status.success() {
        return Err(format!("regia cook exited {status}"));
    }
    Ok(())
}

/// The target dir for the cook's `regia_cli` build when `exe` is a cargo test binary
/// (`<target>/<profile>/deps/<name>-<hash>`): `<target>/third-person-cook`, next to the run's own
/// and cached between runs (as `regia_render`'s `panic-abort` children are). `None` for any other
/// binary: the sample itself shares its target dir, as the developer's `cargo run` does.
fn cook_target_dir(exe: &Path) -> Option<PathBuf> {
    let deps = exe.parent()?;
    if deps.file_name()? != std::ffi::OsStr::new("deps") {
        return None;
    }
    Some(deps.parent()?.parent()?.join("third-person-cook"))
}

/// Loads skeleton, clips and the skinned mesh from the cooked cache.
pub fn load_fox() -> Result<FoxAssets, String> {
    let cache = project_dir()
        .join(".regia")
        .join("cooked")
        .join(std::env::consts::OS);
    let manifest = AssetManifest::from_postcard(
        &std::fs::read(cache.join("asset_manifest.bin")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;

    let mut skeleton: Option<Arc<Skeleton>> = None;
    let mut clips = Vec::new();
    let mut mesh: Option<regia_asset::CpuMesh> = None;
    for entry in &manifest.entries {
        let bytes = std::fs::read(cache.join(format!("{}.fckd", entry.id)))
            .map_err(|e| format!("read {}: {e}", entry.id))?;
        if entry.type_path.contains("Skeleton") {
            let (_, sk) = load_cooked_skeleton(&bytes).map_err(|e| format!("skeleton: {e}"))?;
            skeleton = Some(Arc::new(sk));
        } else if entry.type_path.contains("AnimationClip") {
            let (_, clip) = load_cooked_clip(&bytes).map_err(|e| format!("clip: {e}"))?;
            clips.push(clip);
        } else if entry.type_path.contains("Mesh") && mesh.is_none() {
            let loaded = load_mesh_bytes(&bytes).map_err(|e| format!("mesh: {e}"))?;
            mesh = Some(loaded.mesh);
        }
    }
    let skeleton = skeleton.ok_or("no skeleton cooked from Fox.glb")?;
    let mesh = mesh.ok_or("no mesh cooked from Fox.glb")?;
    let skinned = Arc::new(bind_skin(&mesh, &skeleton)?);
    let min_y = skinned
        .vertices
        .iter()
        .map(|v| v.position[1])
        .fold(f32::INFINITY, f32::min);
    let rig_drop = crate::character::rig_drop(if min_y.is_finite() { min_y } else { 0.0 });
    Ok(FoxAssets {
        skeleton,
        clips,
        mesh: skinned,
        rig_drop,
    })
}

/// The render mesh of the cooked Fox, skinned by its cooked joints and weights.
///
/// The cooked joint indices are ids of the skeleton cooked from the same glTF; the names are
/// compared so a cache from a different import is an error rather than a mis-skinned mesh.
pub fn bind_skin(cooked: &regia_asset::CpuMesh, skeleton: &Skeleton) -> Result<CpuMesh, String> {
    let skin = cooked
        .skin
        .as_ref()
        .ok_or("the cooked Fox mesh has no skin (a cache from before cooked mesh v2: re-cook)")?;
    if skin.joint_names != skeleton.joint_names {
        return Err(format!(
            "cooked skin joints ({}) do not match the cooked skeleton ({})",
            skin.joint_names.len(),
            skeleton.joint_names.len()
        ));
    }
    CpuMesh::from_cooked(cooked).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::load_fox;
    use std::path::Path;

    /// A test binary's cook builds beside the run's target, never into it; the sample binary
    /// keeps its own target dir.
    #[test]
    fn a_test_binary_cooks_with_its_own_target_dir() {
        let exe = Path::new("/w/target/debug/deps/third_person-0e9fb0139faf3e03");
        assert_eq!(
            super::cook_target_dir(exe).as_deref(),
            Some(Path::new("/w/target/third-person-cook"))
        );
        let exe = Path::new("/w/target/release/deps/gameplay-1234");
        assert_eq!(
            super::cook_target_dir(exe).as_deref(),
            Some(Path::new("/w/target/third-person-cook"))
        );
        assert_eq!(
            super::cook_target_dir(Path::new("/w/target/debug/third_person")),
            None
        );
        assert_eq!(super::cook_target_dir(Path::new("third_person")), None);
    }

    /// This very binary is a cargo test binary, so its cook takes the separate dir.
    #[test]
    fn this_test_binary_gets_a_separate_cook_target() {
        let exe = std::env::current_exe().expect("test exe");
        let target = super::cook_target_dir(&exe).expect("a cargo test binary lives in deps/");
        assert!(target.ends_with("third-person-cook"), "{}", target.display());
        assert!(
            !exe.starts_with(&target),
            "{} inside {}",
            exe.display(),
            target.display()
        );
    }

    #[test]
    fn authored_weights_cover_the_cooked_fox() {
        super::ensure_cooked().expect("Fox.glb (set REGIA_KHRONOS_ASSETS or allow the one-time download)");
        let fox = load_fox().expect("cooked fox");
        let count = fox.mesh.vertices.len();
        assert!(count > 1000, "cooked fox has {count} vertices");
        // The Fox's authored skin is almost entirely one or two influences. A
        // distance-based binding fills all four, so a sparse mesh means the glTF
        // weights reached the cooked vertices.
        let sparse = fox
            .mesh
            .vertices
            .iter()
            .filter(|v| v.weights[2] == 0.0 && v.weights[3] == 0.0)
            .count();
        assert!(
            sparse * 10 > count * 8,
            "authored weights on {sparse} of {count} vertices"
        );
        let head = fox.skeleton.joint_index("b_Head_05").expect("head joint");
        let high: Vec<_> = fox
            .mesh
            .vertices
            .iter()
            .filter(|v| v.position[1] > 70.0)
            .collect();
        assert!(!high.is_empty(), "the ears/crown are in the cooked mesh");
        let on_head = high
            .iter()
            .filter(|v| {
                v.joints
                    .iter()
                    .zip(v.weights)
                    .any(|(joint, weight)| *joint == head && weight > 0.5)
            })
            .count();
        assert!(
            on_head * 2 > high.len(),
            "crown vertices follow the head joint ({on_head} of {})",
            high.len()
        );
    }

    /// The cooked skin is the glTF skin: every cooked vertex that sits on a source vertex (raw
    /// or turned by the cooker's 180° yaw) carries that vertex's joints, by name, and its
    /// weights quantised to `unorm16` bit for bit.
    #[test]
    fn cooked_skin_matches_the_gltf_skin() {
        super::ensure_cooked().expect("Fox.glb (set REGIA_KHRONOS_ASSETS or allow the one-time download)");
        let fox = super::load_fox().expect("cooked fox");
        let cache = super::project_dir()
            .join(".regia")
            .join("cooked")
            .join(std::env::consts::OS);
        let manifest = regia_asset::AssetManifest::from_postcard(
            &std::fs::read(cache.join("asset_manifest.bin")).expect("manifest"),
        )
        .expect("manifest");
        let entry = manifest
            .entries
            .iter()
            .find(|e| e.type_path.contains("Mesh"))
            .expect("mesh entry");
        let bytes = std::fs::read(cache.join(format!("{}.fckd", entry.id))).expect("mesh file");
        let cooked = regia_asset::load_mesh_bytes(&bytes).expect("mesh").mesh;
        let skin = cooked.skin.as_ref().expect("cooked skin");

        let glb = std::fs::read(super::project_dir().join("assets/Fox.glb")).expect("Fox.glb");
        let gltf = gltf::Gltf::from_slice(&glb).expect("parse glb");
        let blob = gltf.blob.as_deref().expect("glb blob");
        let joint_names: Vec<String> = gltf
            .skins()
            .next()
            .expect("skin")
            .joints()
            .map(|joint| joint.name().expect("joint name").to_string())
            .collect();
        let prim = gltf
            .meshes()
            .next()
            .expect("mesh")
            .primitives()
            .next()
            .expect("prim");
        let reader = prim.reader(|buffer| (buffer.index() == 0).then_some(blob));
        let positions: Vec<[f32; 3]> = reader.read_positions().expect("positions").collect();
        let joints: Vec<[u16; 4]> = reader.read_joints(0).expect("joints").into_u16().collect();
        let weights: Vec<[f32; 4]> = reader.read_weights(0).expect("weights").into_f32().collect();
        // Seams duplicate a position, so a cooked vertex may match several source vertices.
        let mut by_position: std::collections::HashMap<[u32; 3], Vec<usize>> = Default::default();
        for (i, p) in positions.iter().enumerate() {
            by_position.entry(p.map(f32::to_bits)).or_default().push(i);
            by_position
                .entry([-p[0], p[1], -p[2]].map(f32::to_bits))
                .or_default()
                .push(i);
        }
        let matches = |v: usize, source: usize| {
            let want = regia_asset::quantize_skin_weights(weights[source]);
            skin.weights[v] == want
                && (0..4).all(|slot| {
                    want[slot] == 0
                        || fox.skeleton.joint_names[usize::from(skin.joints[v][slot])]
                            == joint_names[usize::from(joints[source][slot])]
                })
        };
        let mut checked = 0;
        for (v, p) in cooked.positions.iter().enumerate() {
            let Some(sources) = by_position.get(&p.map(f32::to_bits)) else {
                continue;
            };
            assert!(
                sources.iter().any(|&source| matches(v, source)),
                "vertex {v}: joints and weights match no source vertex at its position"
            );
            checked += 1;
        }
        assert!(
            checked * 10 > cooked.positions.len() * 9,
            "{checked} of {} cooked vertices sit on a source vertex",
            cooked.positions.len()
        );
    }
}
