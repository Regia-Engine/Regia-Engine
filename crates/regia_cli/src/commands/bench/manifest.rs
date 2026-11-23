// SPDX-License-Identifier: MPL-2.0
//! The public benchmark scene set: the embedded manifest (`scenes.toml`) and fetching it.
//!
//! Files come from one pinned commit of a public repository, each with its size and SHA-256. A
//! download goes to `<dest>.part` and is renamed only after its checksum matched, so a failed or
//! tampered download never looks fetched. Nothing here runs downloaded content.

use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;

use super::BenchError;

/// The embedded manifest's text.
const MANIFEST: &str = include_str!("scenes.toml");

/// The `version` the manifest carries.
const MANIFEST_VERSION: u32 = 1;

/// The scene set.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// [`MANIFEST_VERSION`].
    pub version: u32,
    /// URL prefix every file's `path` is appended to.
    pub base: String,
    /// The fetched scenes.
    pub scene: Vec<FetchedScene>,
}

/// One scene fetched from the public source.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchedScene {
    /// Stable id (`regia bench <id>`, `docs/benchmark-scenes.md`).
    pub id: String,
    /// Display name.
    pub label: String,
    /// The licence the model's own metadata states.
    pub licence: String,
    /// Who to credit.
    pub credit: String,
    /// `false`: local measurement only, its numbers are never published.
    pub publishable: bool,
    /// The glTF or GLB to load, relative to the asset directory.
    pub entry: String,
    /// Every file of the scene.
    pub file: Vec<SceneFile>,
}

/// One file of a scene.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneFile {
    /// Path relative to the manifest's `base` and to the asset directory.
    pub path: String,
    /// Exact size in bytes.
    pub bytes: u64,
    /// Lowercase hex SHA-256.
    pub sha256: String,
}

impl Manifest {
    /// The embedded manifest, validated.
    ///
    /// # Errors
    /// A manifest that does not parse or validate (a bug in this build, caught by its test).
    pub fn embedded() -> Result<Self, BenchError> {
        Self::parse(MANIFEST)
    }

    /// Parses and validates a manifest.
    ///
    /// # Errors
    /// Bad TOML, another version, a duplicate id, a path that leaves the asset directory, a
    /// malformed checksum, or an entry that is not one of the scene's files.
    pub fn parse(text: &str) -> Result<Self, BenchError> {
        let bad = |message: String| BenchError::new("bench_manifest_invalid", message);
        let manifest: Self = toml::from_str(text).map_err(|err| bad(format!("scenes.toml: {err}")))?;
        if manifest.version != MANIFEST_VERSION {
            return Err(bad(format!("scenes.toml version {}", manifest.version)));
        }
        let mut seen = std::collections::BTreeSet::new();
        for scene in &manifest.scene {
            if !seen.insert(scene.id.as_str()) {
                return Err(bad(format!("duplicate scene id {}", scene.id)));
            }
            for file in &scene.file {
                if !is_safe_relative(&file.path) {
                    return Err(bad(format!("{}: unsafe path {}", scene.id, file.path)));
                }
                let hex = file
                    .sha256
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
                if file.sha256.len() != 64 || !hex {
                    return Err(bad(format!("{}: bad checksum for {}", scene.id, file.path)));
                }
            }
            if !scene.file.iter().any(|file| file.path == scene.entry) {
                return Err(bad(format!("{}: entry {} is not listed", scene.id, scene.entry)));
            }
        }
        Ok(manifest)
    }
}

/// A relative path with only normal components: it stays inside the directory it is joined to.
fn is_safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

/// `true` when the file exists with the manifest's size and checksum.
fn file_ok(path: &Path, file: &SceneFile) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.len() == file.bytes)
        && regia_pt::sha256_file_hex(path).is_ok_and(|hex| hex == file.sha256)
}

impl FetchedScene {
    /// The entry's path under `assets`.
    #[must_use]
    pub fn entry_path(&self, assets: &Path) -> PathBuf {
        assets.join(&self.entry)
    }

    /// `true` when every file is present with its size. The checksum is `--fetch`'s job, since
    /// hashing on each run would land in a bench's page cache.
    #[must_use]
    pub fn is_fetched(&self, assets: &Path) -> bool {
        self.file
            .iter()
            .all(|file| std::fs::metadata(assets.join(&file.path)).is_ok_and(|meta| meta.len() == file.bytes))
    }

    /// Downloads what is missing or wrong and checks every file. Returns how many it downloaded.
    ///
    /// # Errors
    /// `bench_fetch_failed` when a download or a write fails, `bench_checksum_mismatch` when a
    /// download does not match the manifest (the partial file is removed).
    pub fn fetch(&self, base: &str, assets: &Path) -> Result<usize, BenchError> {
        let mut downloaded = 0;
        for file in &self.file {
            let dest = assets.join(&file.path);
            if file_ok(&dest, file) {
                continue;
            }
            download_checked(&format!("{base}{}", url_path(&file.path)), &dest, file)?;
            downloaded += 1;
        }
        Ok(downloaded)
    }
}

/// Percent-encodes a path's spaces, the one character the set's file names need.
fn url_path(path: &str) -> String {
    path.replace(' ', "%20")
}

/// Downloads into `<dest>.part`, checks it, and renames it into place.
fn download_checked(url: &str, dest: &Path, file: &SceneFile) -> Result<(), BenchError> {
    let failed = |message: String| BenchError::new("bench_fetch_failed", message);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|err| failed(format!("{}: {err}", parent.display())))?;
    }
    let mut part = dest.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    let status = Command::new("curl")
        .args(["-fsSL", "--retry", "2", url, "-o"])
        .arg(&part)
        .stdin(Stdio::null())
        .status()
        .map_err(|err| failed(format!("cannot start curl: {err}")))?;
    if !status.success() {
        let _ = std::fs::remove_file(&part);
        return Err(failed(format!("curl failed for {url} ({status})")));
    }
    if !file_ok(&part, file) {
        let _ = std::fs::remove_file(&part);
        return Err(BenchError::new(
            "bench_checksum_mismatch",
            format!("{url} does not match the manifest's size and SHA-256"),
        ));
    }
    std::fs::rename(&part, dest).map_err(|err| failed(format!("{}: {err}", dest.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha256_of(bytes: &[u8], dir: &Path) -> String {
        let path = dir.join("hash-input");
        std::fs::write(&path, bytes).expect("write");
        regia_pt::sha256_file_hex(&path).expect("hash")
    }

    fn scene(file: SceneFile) -> FetchedScene {
        FetchedScene {
            id: "t".into(),
            label: "T".into(),
            licence: "CC0-1.0".into(),
            credit: "t".into(),
            publishable: true,
            entry: file.path.clone(),
            file: vec![file],
        }
    }

    /// The set the owner's rule allows: publishable scenes carry a permissive licence, and every
    /// file is pinned to one commit.
    #[test]
    fn the_embedded_manifest_is_valid_and_publishes_only_permissive_scenes() {
        let manifest = Manifest::embedded().expect("embedded manifest");
        assert!(manifest.scene.len() >= 5, "{}", manifest.scene.len());
        assert!(manifest.base.starts_with("https://"), "{}", manifest.base);
        let commit = manifest.base.split('/').nth(5).unwrap_or_default();
        assert_eq!(commit.len(), 40, "base must pin a commit: {}", manifest.base);
        for scene in &manifest.scene {
            if scene.publishable {
                assert!(
                    scene.licence.starts_with("CC0") || scene.licence.starts_with("CC-BY"),
                    "{} publishes under {}",
                    scene.id,
                    scene.licence
                );
                assert!(!scene.licence.contains("-NC"), "{}", scene.id);
                assert!(!scene.credit.is_empty(), "{}", scene.id);
            }
        }
    }

    #[test]
    fn restricted_models_stay_unpublishable() {
        let manifest = Manifest::embedded().expect("embedded manifest");
        let sponza = manifest
            .scene
            .iter()
            .find(|scene| scene.id == "sponza_khronos")
            .expect("sponza row");
        assert!(!sponza.publishable);
        assert!(sponza.licence.contains("CRYENGINE"), "{}", sponza.licence);
    }

    #[test]
    fn unsafe_paths_are_refused() {
        for path in ["../up", "/abs", "a/../../b", "", "a\\b"] {
            assert!(!is_safe_relative(path), "{path}");
        }
        assert!(is_safe_relative("Model/glTF/a b.png"));
    }

    fn manifest_text(path: &str, sha: &str, entry: &str) -> String {
        format!(
            "version = 1\nbase = \"file:///x/\"\n[[scene]]\nid = \"a\"\nlabel = \"A\"\nlicence = \"CC0-1.0\"\ncredit = \"c\"\npublishable = true\nentry = \"{entry}\"\n[[scene.file]]\npath = \"{path}\"\nbytes = 1\nsha256 = \"{sha}\"\n"
        )
    }

    #[test]
    fn bad_manifests_are_refused() {
        let good = "0".repeat(64);
        assert!(Manifest::parse(&manifest_text("a.glb", &good, "a.glb")).is_ok());
        let code = |text: String| Manifest::parse(&text).map(|_| ()).map_err(|err| err.code);
        assert_eq!(
            code(manifest_text("../a.glb", &good, "../a.glb")),
            Err("bench_manifest_invalid")
        );
        assert_eq!(
            code(manifest_text("a.glb", "XYZ", "a.glb")),
            Err("bench_manifest_invalid")
        );
        assert_eq!(
            code(manifest_text("a.glb", &good, "b.glb")),
            Err("bench_manifest_invalid")
        );
    }

    /// The whole fetch through curl's `file://` path, so no network: a good file lands, a second
    /// fetch has nothing to do, and a wrong checksum leaves nothing behind.
    #[test]
    fn fetch_checks_the_file_and_never_keeps_a_bad_one() {
        let dir = regia_test_utils::unique_temp_dir("bench_fetch");
        let source = dir.join("source");
        std::fs::create_dir_all(&source).expect("mkdir");
        std::fs::write(source.join("model.glb"), b"payload").expect("write");
        let base = format!("file://{}/", source.display());
        let assets = dir.join("assets");
        let good = scene(SceneFile {
            path: "model.glb".into(),
            bytes: 7,
            sha256: sha256_of(b"payload", &dir),
        });
        assert!(!good.is_fetched(&assets));
        assert_eq!(good.fetch(&base, &assets).expect("fetch"), 1);
        assert!(good.is_fetched(&assets));
        assert_eq!(good.fetch(&base, &assets).expect("second fetch"), 0);

        let bad = scene(SceneFile {
            path: "bad/model.glb".into(),
            bytes: 7,
            sha256: "0".repeat(64),
        });
        std::fs::create_dir_all(source.join("bad")).expect("mkdir");
        std::fs::write(source.join("bad/model.glb"), b"payload").expect("write");
        let err = bad.fetch(&base, &assets).expect_err("checksum must fail");
        assert_eq!(err.code, "bench_checksum_mismatch");
        assert!(!assets.join("bad/model.glb").exists());
        assert!(!assets.join("bad/model.glb.part").exists());

        let gone = scene(SceneFile {
            path: "gone.glb".into(),
            bytes: 7,
            sha256: "0".repeat(64),
        });
        assert_eq!(
            gone.fetch(&base, &assets).expect_err("no file").code,
            "bench_fetch_failed"
        );
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
