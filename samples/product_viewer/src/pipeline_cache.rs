// SPDX-License-Identifier: MIT-0
//! The driver's compiled pipelines, kept in a file between runs. On a slow phone GPU most of the wait for the
//! first frame is the driver compiling the viewer's pipelines (5.4 s of 5.9 s on an Adreno 504), and without a
//! cache every launch pays it again. Vulkan only: elsewhere wgpu has no such cache and this is not used. ADR-2835.

use std::path::{Path, PathBuf};

/// The cache of this adapter and the file it came from.
#[derive(Debug)]
pub struct PipelineCacheFile {
    path: PathBuf,
    cache: wgpu::PipelineCache,
    /// What the file held when it was read, to skip writing the same bytes back.
    loaded: Vec<u8>,
}

impl PipelineCacheFile {
    /// Opens the cache file of `adapter` in `dir`; a missing or unusable file means an empty cache. `None` when
    /// the device has no pipeline cache (not Vulkan).
    #[must_use]
    pub fn open(dir: &Path, adapter: &wgpu::Adapter, device: &wgpu::Device) -> Option<Self> {
        if !device.features().contains(wgpu::Features::PIPELINE_CACHE) {
            return None;
        }
        // The name holds the adapter and driver, so a driver update starts a new file.
        let path = dir.join(wgpu::util::pipeline_cache_key(&adapter.get_info())?);
        let loaded = std::fs::read(&path).unwrap_or_default();
        // SAFETY: the bytes are read from the app's private folder, where only `save` below writes them, by
        // tmp + rename, from `get_data` of a cache of this same adapter and driver (the file name says which).
        // wgpu checks their header against this adapter, and `fallback` turns anything it refuses into an
        // empty cache. ADR-2835.
        #[allow(unsafe_code)]
        let cache = unsafe {
            device.create_pipeline_cache(&wgpu::PipelineCacheDescriptor {
                label: Some("regia viewer pipelines"),
                data: (!loaded.is_empty()).then_some(loaded.as_slice()),
                fallback: true,
            })
        };
        Some(Self { path, cache, loaded })
    }

    /// The cache, for the renderer's configuration.
    #[must_use]
    pub fn cache(&self) -> &wgpu::PipelineCache {
        &self.cache
    }

    /// The size of the file when it was read; 0 when there was none.
    #[must_use]
    pub fn loaded_bytes(&self) -> usize {
        self.loaded.len()
    }

    /// Writes the cache back when it changed. Returns the bytes written.
    ///
    /// # Errors
    /// The folder or the file could not be written. Nothing depends on the file: the next run compiles again.
    pub fn save(&self) -> std::io::Result<usize> {
        let Some(data) = self.cache.get_data() else {
            return Ok(0);
        };
        if data == self.loaded {
            return Ok(0);
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, &data)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(data.len())
    }
}
