// SPDX-License-Identifier: MPL-2.0
//! Saved pipeline-cache attachments. No driver calls or directory scans (ADR-8162).

use std::io::Read;
use std::path::PathBuf;

use regia_app::prelude::Resource;
use regia_ecs::ecs as bevy_ecs;
use regia_render::pipeline_cache::{self, PipelineCacheKey};

/// A saved cache to attach to feedback. Hosts with an external renderer may insert this
/// resource; otherwise capture uses the installed renderer's cache path and exact key.
/// Reading and validation happen on the feedback writer, never on the frame path.
#[derive(Debug, Clone, Resource)]
pub struct PipelineCacheAttachment {
    /// Local cache file saved by the renderer. Never a directory to scan.
    pub path: PathBuf,
    /// Expected GPU, driver, wgpu and shader identity at capture time.
    pub key: PipelineCacheKey,
}

impl PipelineCacheAttachment {
    /// Reads an intact saved cache within `max_bytes`, also capped to half the bundle
    /// reader's size limit. Missing, stale, invalid or oversized files are omitted by
    /// the bundle writer with this error as its diagnostic; the feedback itself survives.
    pub(super) fn read(&self, max_bytes: u64) -> Result<Vec<u8>, String> {
        let cap = max_bytes.min(super::bundle::MAX_BUNDLE_BYTES / 2);
        if cap == 0 {
            return Err("pipeline cache attachment disabled".into());
        }
        // Only Vulkan has persistent cache blobs. This also keeps the generated archive
        // name independent of an arbitrary backend string in a dropped-in file.
        if self.key.backend != "Vulkan" {
            return Err("pipeline cache backend is not Vulkan".into());
        }
        let file = std::fs::File::open(&self.path).map_err(|e| format!("saved cache unavailable: {e}"))?;
        let meta = file
            .metadata()
            .map_err(|e| format!("saved cache metadata: {e}"))?;
        if !meta.is_file() || meta.len() > cap {
            return Err("saved cache is not a regular file within the attachment budget".into());
        }
        let mut bytes = Vec::new();
        file.take(cap + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("saved cache read: {e}"))?;
        if bytes.len() as u64 > cap {
            return Err("saved cache exceeds the attachment budget".into());
        }
        pipeline_cache::decode(&bytes, &self.key).map_err(|e| format!("saved cache rejected: {e}"))?;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feedback::{BundleInput, FeedbackCategory, build_bundle, zip::read_entries};

    #[test]
    fn feedback_pipeline_cache_is_validated_and_failures_preserve_the_report() {
        let root = regia_test_utils::unique_temp_dir("feedback-fpc");
        let key = PipelineCacheKey {
            backend: "Vulkan".into(),
            vendor: 1,
            device: 2,
            driver: "test".into(),
            driver_info: "1".into(),
            wgpu_version: pipeline_cache::WGPU_VERSION.into(),
            shader_hash: "test-content".into(),
        };
        let path = root.join("saved.fpc");
        let good = pipeline_cache::encode(&key, &[1; 256]);
        regia_test_utils::write_atomic(&path, &good).expect("saved cache");
        let mut input = BundleInput::new(FeedbackCategory::Performance, "stutter");
        input.pipeline_cache = Some(PipelineCacheAttachment {
            path: path.clone(),
            key: key.clone(),
        });
        let (bytes, names) = build_bundle(&input);
        let name = format!("pipelines/{}", key.shipped_file_name());
        assert!(names.contains(&name));
        let entries = read_entries(&bytes).expect("bundle");
        assert_eq!(
            entries.iter().find(|entry| entry.name == name).expect("fpc").data,
            good
        );
        let meta: serde_json::Value = serde_json::from_slice(&entries[0].data).expect("metadata");
        assert_eq!(meta["pipeline_cache"], name);
        assert!(
            meta["files"]
                .as_array()
                .expect("files")
                .iter()
                .any(|value| value == &name)
        );
        assert!(meta["pipeline_cache_missing"].is_null());

        // Independent failures: disabled, too large, another driver, corrupt, missing.
        for case in 0..5 {
            input.pipeline_cache_max_bytes = 32 * 1024 * 1024;
            let mut bytes = good.clone();
            match case {
                0 => input.pipeline_cache_max_bytes = 0,
                1 => input.pipeline_cache_max_bytes = good.len() as u64 - 1,
                2 => {
                    bytes = pipeline_cache::encode(
                        &PipelineCacheKey {
                            driver_info: "2".into(),
                            ..key.clone()
                        },
                        &[1; 256],
                    )
                }
                3 => *bytes.last_mut().expect("payload") ^= 1,
                _ => {}
            }
            regia_test_utils::write_atomic(&path, &bytes).expect("fixture");
            if case == 4 {
                std::fs::remove_file(&path).expect("remove");
            }
            let (archive, names) = build_bundle(&input);
            assert!(!names.iter().any(|name| name.ends_with(".fpc")), "case {case}");
            assert!(names.iter().any(|name| name == "note.txt"));
            let entries = read_entries(&archive).expect("report survives");
            let meta: serde_json::Value = serde_json::from_slice(&entries[0].data).expect("metadata");
            assert!(meta["pipeline_cache"].is_null());
            assert!(meta["pipeline_cache_missing"].is_string());
        }
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
