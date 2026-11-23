// SPDX-License-Identifier: MPL-2.0
//! The renderer's named CPU heap owners (ADR-3351).
//!
//! Each `static` here is a [`MemoryOwner`] inside [`Subsystem::Render`]: with the tagging allocator
//! installed (`regia_core::memory::TaggingAllocator`: dev builds, tests, soaks), the bytes a phase
//! of the renderer allocates and keeps count against its row, so `memory.report` and
//! `memory.idle_ledger` name what holds the renderer's heap instead of one `render` total (or
//! `unscoped`, for start-up work outside the frame). Without the allocator an owner scope costs two
//! thread-local stores.
//!
//! An owner is entered where the work happens, outside `frame.rs` and `frame/**`: the frame's own
//! `Render` scope keeps whichever owner the caller entered ([`regia_core::memory::enter`]), and the
//! inner owners (pipelines, shader modules) win over the outer ones while they run. Bytes the frame
//! keeps outside every inner owner show as [`FRAME`].

use regia_core::memory::{MemoryOwner, Subsystem};

/// `RenderPlugin::build`: resources, schedules, type registrations.
pub static PLUGIN: MemoryOwner = MemoryOwner::new(Subsystem::Render, "render.plugin");
/// The wgpu instance, adapter and device (`RenderDevice::new*`, `create_instance`).
pub static DEVICE: MemoryOwner = MemoryOwner::new(Subsystem::Render, "render.device");
/// WGSL composition (the per-thread composer over the engine library and its cached imports)
/// and the composed modules the shader modules keep.
pub static SHADER_MODULES: MemoryOwner = MemoryOwner::new(Subsystem::Render, "render.shader_modules");
/// Render and compute pipeline creation (`TrackedDevice::create_*_pipeline`): the pipeline
/// objects and their backend compile.
pub static PIPELINES: MemoryOwner = MemoryOwner::new(Subsystem::Render, "render.pipelines");
/// The persistent pipeline cache: its file payload, load and save (`pipeline_cache.rs`).
pub static PIPELINE_CACHE: MemoryOwner = MemoryOwner::new(Subsystem::Render, "render.pipeline_cache");
/// Sky and atmosphere tables (transmittance, multiple scattering, sky view; `atmosphere.rs`).
pub static ATMOSPHERE_LUTS: MemoryOwner = MemoryOwner::new(Subsystem::Render, "render.atmosphere_luts");
/// The spatiotemporal blue-noise texture (`blue_noise.rs`).
pub static BLUE_NOISE: MemoryOwner = MemoryOwner::new(Subsystem::Render, "render.blue_noise");
/// Color-grading LUTs: level LUTs, post-volume LUTs, the color-vision LUT (`lut.rs`,
/// `post_volume_luts.rs`).
pub static COLOR_LUTS: MemoryOwner = MemoryOwner::new(Subsystem::Render, "render.color_luts");
/// Cloud noise volumes and the curl field (`clouds.rs`).
pub static CLOUD_NOISE: MemoryOwner = MemoryOwner::new(Subsystem::Render, "render.cloud_noise");
/// Extraction of the ECS into the frame description (`runtime::extract_frame` and the extract
/// systems after it).
pub static EXTRACT: MemoryOwner = MemoryOwner::new(Subsystem::Render, "render.extract");
/// The background upload queue (`upload_queue.rs`).
pub static UPLOADS: MemoryOwner = MemoryOwner::new(Subsystem::Render, "render.upload_queue");
/// The frame (texture views and windows): what the frame renderer keeps outside every owner above.
pub static FRAME: MemoryOwner = MemoryOwner::new(Subsystem::Render, "render.frame");

/// Every renderer owner, in report order.
pub static ALL: [&MemoryOwner; 12] = [
    &PLUGIN,
    &DEVICE,
    &SHADER_MODULES,
    &PIPELINES,
    &PIPELINE_CACHE,
    &ATMOSPHERE_LUTS,
    &BLUE_NOISE,
    &COLOR_LUTS,
    &CLOUD_NOISE,
    &EXTRACT,
    &UPLOADS,
    &FRAME,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_names_are_unique_render_names() {
        let mut names: Vec<&str> = ALL.iter().map(|o| o.name()).collect();
        for (o, name) in ALL.iter().zip(&names) {
            assert_eq!(o.subsystem(), Subsystem::Render, "{name}");
            assert!(name.starts_with("render."), "{name}");
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'_' || b == b'.'),
                "{name}"
            );
        }
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ALL.len());
        assert!(ALL.len() < regia_core::memory::MAX_OWNERS / 2);
    }
}
