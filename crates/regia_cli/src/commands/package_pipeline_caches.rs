// SPDX-License-Identifier: MPL-2.0
//! Bounded feedback ZIP harvesting, derived from the requirements of
//! DO27 tools/ops/fpc_collect.py at d6ed37677 (regia-absorb-1), rewritten in Rust.
//! Existing Regia FPCH/ZIP decoders own the formats; no driver blobs are combined.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use regia_framework::feedback::{bundle::MAX_BUNDLE_BYTES, zip::read_entries};
use regia_render::pipeline_cache::{self, FPCH_MAX_BYTES, PipelineCacheKey};

/// Location and digest of a validated candidate. Rechecked immediately before staging,
/// so a file replaced between collection and copying cannot bypass validation.
#[derive(Debug, Clone)]
pub struct PipelineCacheSource {
    /// Loose cache or feedback ZIP on disk.
    pub path: PathBuf,
    /// Entry in a stored ZIP, or `None` for a loose cache. Never joined to a disk path.
    pub entry: Option<String>,
    digest: blake3::Hash,
}

impl PipelineCacheSource {
    /// Reads exactly the bytes validated during collection, or fails if they changed.
    pub fn read(&self) -> Result<Vec<u8>> {
        let bytes = if let Some(name) = &self.entry {
            let archive = read_capped(&self.path, MAX_BUNDLE_BYTES)?;
            read_entries(&archive)
                .map_err(anyhow::Error::msg)?
                .into_iter()
                .find(|entry| &entry.name == name)
                .with_context(|| format!("{}: missing cache entry {name}", self.path.display()))?
                .data
        } else {
            read_capped(&self.path, FPCH_MAX_BYTES)?
        };
        if blake3::hash(&bytes) != self.digest {
            bail!("pipeline cache {} changed after collection", self.path.display());
        }
        Ok(bytes)
    }
}

/// Shipped filename to source, exact compatibility key and encoded size.
pub type CollectedCaches = BTreeMap<String, (PipelineCacheSource, PipelineCacheKey, u64)>;

/// Stages validated bytes with a unique same-directory temporary file and atomic rename.
/// The parent directory must exist. A failed write/rename preserves the previous destination.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut name = path
        .file_name()
        .context("pipeline cache destination has no filename")?
        .to_os_string();
    name.push(format!(
        ".tmp.{}.{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let temp = path.with_file_name(name);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file); // Windows must release the temporary file before the rename.
    let result = written.and_then(|()| std::fs::rename(&temp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.with_context(|| format!("staging {}", path.display()))
}

fn read_capped(path: &Path, cap: u64) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > cap {
        bail!(
            "{} is not a regular file within the {cap}-byte cache input limit",
            path.display()
        );
    }
    let mut bytes = Vec::new();
    file.take(cap + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > cap {
        bail!("{} grew past the {cap}-byte cache input limit", path.display());
    }
    Ok(bytes)
}

/// Collects a loose `.fpc`, stored feedback ZIP, or directory containing either recursively.
/// Invalid inputs fail closed; non-cache files are ignored. Symlinks are not followed.
pub fn collect(root: &Path) -> Result<CollectedCaches> {
    let mut paths = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .follow_root_links(false)
    {
        let entry = entry.with_context(|| format!("reading cache input {}", root.display()))?;
        let relevant = entry
            .path()
            .extension()
            .is_some_and(|ext| ext == "fpc" || ext == "zip");
        if entry.depth() == 0 && !entry.file_type().is_dir() && !relevant {
            bail!(
                "pipeline cache input {} must be a .fpc, .zip or directory",
                root.display()
            );
        }
        if entry.file_type().is_symlink() {
            if relevant || entry.path() == root {
                bail!(
                    "pipeline cache input {} is a symbolic link",
                    entry.path().display()
                );
            }
            continue;
        }
        if relevant && !entry.file_type().is_dir() {
            if !entry.file_type().is_file() {
                bail!(
                    "pipeline cache input {} is not a regular file",
                    entry.path().display()
                );
            }
            paths.push(entry.into_path());
        }
    }
    paths.sort();
    let mut collected = BTreeMap::new();
    for path in paths {
        if path.extension().is_some_and(|ext| ext == "zip") {
            let archive = read_capped(&path, MAX_BUNDLE_BYTES)?;
            let entries = read_entries(&archive)
                .map_err(anyhow::Error::msg)
                .with_context(|| format!("pipeline cache bundle {}", path.display()))?;
            for entry in entries {
                if entry.name.ends_with(".fpc") {
                    insert(&mut collected, &path, Some(entry.name), &entry.data)?;
                }
            }
        } else {
            let bytes = read_capped(&path, FPCH_MAX_BYTES)?;
            insert(&mut collected, &path, None, &bytes)?;
        }
    }
    Ok(collected)
}

fn insert(collected: &mut CollectedCaches, path: &Path, entry: Option<String>, bytes: &[u8]) -> Result<()> {
    let key = pipeline_cache::read_key(bytes)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("pipeline cache {} {entry:?}", path.display()))?;
    pipeline_cache::decode(bytes, &key)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("pipeline cache {} {entry:?}", path.display()))?;
    // The filename incorporates the backend: never let a forged key introduce a path.
    if key.backend != "Vulkan" {
        bail!(
            "pipeline cache {} {entry:?}: backend must be Vulkan",
            path.display()
        );
    }
    let name = key.shipped_file_name();
    let digest = blake3::hash(bytes);
    let size = bytes.len() as u64;
    if let Some((source, existing, len)) = collected.get(&name) {
        if existing != &key {
            bail!("pipeline cache shipped-name collision: {name}");
        }
        // Largest intact blob wins. Equal sizes choose the lowest content hash, independent
        // of directory/ZIP order. Never concatenate opaque driver payloads.
        if size < *len || (size == *len && digest.as_bytes() >= source.digest.as_bytes()) {
            return Ok(());
        }
    }
    collected.insert(
        name,
        (
            PipelineCacheSource {
                path: path.to_path_buf(),
                entry,
                digest,
            },
            key,
            size,
        ),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::feedback::zip::ZipWriter;
    use regia_render::pipeline_cache::{WGPU_VERSION, encode, engine_shader_hash};

    fn key() -> PipelineCacheKey {
        PipelineCacheKey {
            backend: "Vulkan".into(),
            vendor: 1,
            device: 2,
            driver: "test".into(),
            driver_info: "1".into(),
            wgpu_version: WGPU_VERSION.into(),
            shader_hash: engine_shader_hash(),
        }
    }

    #[test]
    fn pipeline_caches_preserve_the_collectors_hash_vectors() {
        // The two mandatory selftest vectors in the requirements source, d6ed37677.
        // Regia reuses blake3 instead of carrying the Python one-chunk hash implementation.
        for (input, expected) in [
            (
                b"".as_slice(),
                "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
            ),
            (
                b"abc".as_slice(),
                "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85",
            ),
        ] {
            assert_eq!(blake3::hash(input).to_hex().as_str(), expected);
        }
    }

    #[test]
    fn pipeline_caches_atomic_stage_cleans_up_after_success_and_failure() -> Result<()> {
        let root = regia_test_utils::unique_temp_dir("fpc-atomic-stage");
        let path = root.join("cache.fpc");
        write_atomic(&path, b"first")?;
        write_atomic(&path, b"second")?;
        assert_eq!(std::fs::read(&path)?, b"second");
        let directory = root.join("directory.fpc");
        std::fs::create_dir(&directory)?;
        assert!(write_atomic(&directory, b"cannot replace a directory").is_err());
        assert!(directory.is_dir());
        assert_eq!(std::fs::read_dir(&root)?.count(), 2, "no temporary files remain");
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn pipeline_caches_ties_and_nested_inputs_are_order_independent() -> Result<()> {
        let root = regia_test_utils::unique_temp_dir("fpc-ties");
        std::fs::create_dir(root.join("nested"))?;
        let a = encode(&key(), &[1; 100]);
        let b = encode(&key(), &[2; 100]);
        let expected = if blake3::hash(&a).as_bytes() < blake3::hash(&b).as_bytes() {
            &a
        } else {
            &b
        };
        for reverse in [false, true] {
            let mut zip = ZipWriter::new(0);
            let (first, second) = if reverse { (&a, &b) } else { (&b, &a) };
            zip.add("a.fpc", first);
            zip.add("b.fpc", second);
            regia_test_utils::write_atomic(&root.join("nested/feedback.zip"), &zip.finish())?;
            regia_test_utils::write_atomic(&root.join("loose.fpc"), first)?;
            let got = collect(&root)?;
            assert_eq!(got.len(), 1);
            assert_eq!(&got[&key().shipped_file_name()].0.read()?, expected);
        }
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn pipeline_caches_keep_gpu_driver_shader_and_wgpu_keys_separate() -> Result<()> {
        let root = regia_test_utils::unique_temp_dir("fpc-all-key-fields");
        let keys = [
            key(),
            PipelineCacheKey { vendor: 3, ..key() },
            PipelineCacheKey { device: 3, ..key() },
            PipelineCacheKey {
                driver: "other-driver".into(),
                ..key()
            },
            PipelineCacheKey {
                driver_info: "2".into(),
                ..key()
            },
            PipelineCacheKey {
                shader_hash: "other-content".into(),
                ..key()
            },
            PipelineCacheKey {
                wgpu_version: "other-version".into(),
                ..key()
            },
        ];
        let mut zip = ZipWriter::new(0);
        for (index, key) in keys.iter().enumerate() {
            zip.add(&format!("{index}.fpc"), &encode(key, &[index as u8; 100]));
        }
        regia_test_utils::write_atomic(&root.join("all.zip"), &zip.finish())?;
        let collected = collect(&root)?;
        assert_eq!(collected.len(), keys.len());
        for (index, key) in keys.iter().enumerate() {
            let bytes = collected[&key.shipped_file_name()].0.read()?;
            assert_eq!(
                pipeline_cache::decode(&bytes, key).map_err(anyhow::Error::msg)?,
                &[index as u8; 100]
            );
        }
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn pipeline_caches_reject_corruption_and_changed_sources() -> Result<()> {
        let root = regia_test_utils::unique_temp_dir("fpc-corrupt");
        let path = root.join("cache.fpc");
        let good = encode(&key(), &[1; 100]);
        regia_test_utils::write_atomic(&path, &good)?;
        let got = collect(&path)?;
        // Even another valid blob under the same key cannot race the validated selection.
        regia_test_utils::write_atomic(&path, &encode(&key(), &[2; 100]))?;
        assert!(got[&key().shipped_file_name()].0.read().is_err());
        let mut broken = good;
        *broken.last_mut().expect("payload") ^= 1;
        let mut zip = ZipWriter::new(0);
        zip.add("bad.fpc", &broken); // valid ZIP CRC, invalid FPCH checksum
        let path = root.join("feedback.zip");
        regia_test_utils::write_atomic(&path, &zip.finish())?;
        assert!(collect(&path).is_err());
        regia_test_utils::write_atomic(&path, b"not a zip")?;
        assert!(collect(&path).is_err());
        let other = root.join("not-a-cache.txt");
        regia_test_utils::write_atomic(&other, b"not a cache input")?;
        assert!(collect(&other).is_err());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn pipeline_caches_reject_oversized_files_and_unsafe_names() -> Result<()> {
        let root = regia_test_utils::unique_temp_dir("fpc-bounds");
        for (name, cap) in [("large.zip", MAX_BUNDLE_BYTES), ("large.fpc", FPCH_MAX_BYTES)] {
            let path = root.join(name);
            std::fs::File::create(&path)?.set_len(cap + 1)?;
            assert!(collect(&path).is_err());
            std::fs::remove_file(path)?;
        }
        let mut zip = ZipWriter::new(0);
        zip.add("cache/a.fpc", &encode(&key(), &[1; 100]));
        let mut bytes = zip.finish();
        // Replace both local and central names; ZIP data/CRC stays intact.
        for at in 0..bytes.len() - 10 {
            if &bytes[at..at + 11] == b"cache/a.fpc" {
                bytes[at..at + 11].copy_from_slice(b"../evil.fpc");
            }
        }
        let path = root.join("unsafe.zip");
        regia_test_utils::write_atomic(&path, &bytes)?;
        assert!(collect(&path).is_err());
        let unsafe_key = PipelineCacheKey {
            backend: "../../outside".into(),
            ..key()
        };
        let path = root.join("unsafe.fpc");
        regia_test_utils::write_atomic(&path, &encode(&unsafe_key, &[1; 100]))?;
        assert!(collect(&path).is_err());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn pipeline_caches_do_not_follow_symlinks() -> Result<()> {
        let root = regia_test_utils::unique_temp_dir("fpc-symlink");
        let file = root.join("cache.fpc");
        regia_test_utils::write_atomic(&file, &encode(&key(), &[1; 100]))?;
        let link = root.join("linked.fpc");
        std::os::unix::fs::symlink(&file, &link)?;
        assert!(collect(&link).is_err());
        assert!(collect(&root).is_err());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
