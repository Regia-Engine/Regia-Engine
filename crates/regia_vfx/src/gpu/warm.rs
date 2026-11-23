// SPDX-License-Identifier: MPL-2.0
//! Pipeline warm-up for effects: builds a [`VfxGpu`]'s render pipelines (billboards, mesh
//! particles, decals, trails, the soft-layer composite) for the colour formats the frame draws
//! effects into, before the first effect spawns, so the frame path never compiles one
//! (`docs/specs/13-rendering.md` § 2b).
//!
//! [`VfxGpu::warm`] builds them in place (a loading screen). [`vfx_warm_job`] is the same as a
//! [`regia_render::PipelineWarmQueue`] job on a context shared with the frame hook: it
//! compiles outside the context's lock (the frame keeps drawing and uploading meanwhile) and
//! takes the lock only to store each finished set. Both stop between compiles once the queue
//! shuts down ([`regia_render::warm_cancelled`], ADR-170), and every pipeline goes through
//! the installed (and shipped) pipeline cache like every engine pipeline.

use std::sync::{Arc, Mutex};

use regia_render::pipeline_warm::WarmJob;

use super::soft::{LAYER_FORMAT, SoftCompositeSource};
use super::{PipeSource, RenderPipes, VfxError, VfxGpu};

/// What a VFX warm ([`VfxGpu::warm`], [`warm_shared`]) built.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VfxWarmReport {
    /// Render pipelines compiled (formats built before cost none).
    pub pipelines: u32,
    /// The warm stopped early because its [`regia_render::PipelineWarmQueue`] is shutting
    /// down; what it finished before is kept.
    pub canceled: bool,
}

/// What a warm still has to build, taken from the context in one short lock.
struct WarmPlan {
    particles: PipeSource,
    /// Formats without a particle/decal/trail set.
    formats: Vec<wgpu::TextureFormat>,
    /// The soft-layer composite and the formats without one.
    soft: Option<(SoftCompositeSource, Vec<wgpu::TextureFormat>)>,
}

/// One finished build, stored into the context.
enum Built {
    Particles(wgpu::TextureFormat, RenderPipes),
    Composite(wgpu::TextureFormat, wgpu::RenderPipeline),
}

impl VfxGpu {
    /// [`Self::new`] followed by [`Self::warm`] for `formats`: a context whose compute
    /// pipelines (simulation, sort, radix sort) and render sets are all built. Run it on a
    /// [`regia_render::PipelineWarmQueue`] worker (the ECS runtime does, ADR-195) and hand
    /// the context to the frame: nothing compiles on the frame thread.
    ///
    /// `Ok(None)` when it stopped early because the queue is shutting down (the app is
    /// quitting): it checks between its compile batches ([`regia_render::warm_cancelled`]),
    /// so a quit does not wait for the whole build. Never `None` off the warm workers.
    ///
    /// # Errors
    ///
    /// As [`Self::new`] and [`Self::warm`].
    pub fn new_warmed(
        device: &regia_render::RenderDevice,
        profile: super::VfxProfile,
        formats: &[wgpu::TextureFormat],
    ) -> Result<Option<Self>, VfxError> {
        let _span = tracing::info_span!("vfx_new_warmed").entered();
        let Some(mut gpu) = Self::build(device, profile)? else {
            return Ok(None);
        };
        if gpu.warm(formats)?.canceled {
            return Ok(None);
        }
        Ok(Some(gpu))
    }

    /// The colour formats the frame path draws effects into, for a window (or offscreen
    /// target) of format `window`: `window` itself (the display pass: MSAA frames, `Rg11b10`
    /// HDR, frames without post), `Rgba16Float` (the HDR pass, the TAAU output and the
    /// soft layer) and the format of every target the host registered for the warm
    /// ([`regia_render::pipeline_warm::register_warm_target`]: a render-to-texture camera, a
    /// second window; ADR-7885). Pass the result to [`Self::warm`] or [`vfx_warm_job`].
    #[must_use]
    pub fn warm_formats(window: wgpu::TextureFormat) -> Vec<wgpu::TextureFormat> {
        let mut formats = vec![window];
        if window != LAYER_FORMAT {
            formats.push(LAYER_FORMAT);
        }
        for format in regia_render::pipeline_warm::registered_warm_formats() {
            if !formats.contains(&format) {
                formats.push(format);
            }
        }
        formats
    }

    /// Builds every effect render pipeline for each of `formats` now: the haze, additive and
    /// sorted-alpha billboards, mesh particles, projected decals and trails, plus the soft
    /// layer's composite (`Rgba16Float`) when the profile uses the layer. Afterwards the first
    /// emitter of any kind, decal or trail drawn into one of `formats` compiles nothing on the
    /// frame path. Formats already built cost nothing, so calling it again is cheap.
    ///
    /// On a [`regia_render::PipelineWarmQueue`] worker it stops between two compiles once the
    /// queue shuts down ([`VfxWarmReport::canceled`]). A context shared with the frame hook
    /// is warmed with [`vfx_warm_job`] / [`warm_shared`] instead, which do not hold its lock
    /// while compiling.
    ///
    /// # Errors
    ///
    /// A pipeline failed validation (a shader bug); the sets finished before it are kept.
    pub fn warm(&mut self, formats: &[wgpu::TextureFormat]) -> Result<VfxWarmReport, VfxError> {
        let plan = self.warm_plan(formats)?;
        run_plan(plan, |built| self.store(built))
    }

    /// The builds `formats` still need.
    fn warm_plan(&mut self, formats: &[wgpu::TextureFormat]) -> Result<WarmPlan, VfxError> {
        let mut wanted: Vec<wgpu::TextureFormat> = Vec::new();
        for &format in formats {
            if !wanted.contains(&format) {
                wanted.push(format);
            }
        }
        let soft_on = self.soft_layer_in_profile();
        // The soft layer draws the alpha list into its own `Rgba16Float` target.
        if soft_on && !wanted.contains(&LAYER_FORMAT) {
            wanted.push(LAYER_FORMAT);
        }
        let missing: Vec<_> = wanted
            .iter()
            .copied()
            .filter(|format| !self.render_pipes.contains_key(format))
            .collect();
        // The composite draws only into the HDR pass's `Rgba16Float` target.
        let soft = if soft_on && !self.has_soft_composite(LAYER_FORMAT) {
            Some((self.soft_composite_source()?, vec![LAYER_FORMAT]))
        } else {
            None
        };
        Ok(WarmPlan {
            particles: self.pipe_source(),
            formats: missing,
            soft,
        })
    }

    /// Keeps a finished build (a set the frame path built meanwhile wins).
    fn store(&mut self, built: Built) {
        match built {
            Built::Particles(format, pipes) => {
                self.render_pipes.entry(format).or_insert(pipes);
            }
            Built::Composite(format, pipe) => self.insert_soft_composite(format, pipe),
        }
    }
}

/// Compiles `plan`, handing each finished build to `store`.
fn run_plan(plan: WarmPlan, mut store: impl FnMut(Built)) -> Result<VfxWarmReport, VfxError> {
    let _span = tracing::info_span!("vfx_pipeline_warm").entered();
    let mut report = VfxWarmReport::default();
    for format in plan.formats {
        if regia_render::warm_cancelled() {
            report.canceled = true;
            return Ok(report);
        }
        match plan.particles.build(format, true)? {
            Some(pipes) => {
                report.pipelines += PipeSource::PIPELINES;
                store(Built::Particles(format, pipes));
            }
            None => {
                report.canceled = true;
                return Ok(report);
            }
        }
    }
    if let Some((source, formats)) = plan.soft {
        for format in formats {
            if regia_render::warm_cancelled() {
                report.canceled = true;
                return Ok(report);
            }
            let pipe = source.build(format)?;
            report.pipelines += 1;
            store(Built::Composite(format, pipe));
        }
    }
    Ok(report)
}

/// [`VfxGpu::warm`] on a context shared with the frame hook (the `Arc<Mutex<VfxGpu>>` given to
/// [`crate::translucent_layer`], or [`crate::runtime::VfxGpuHandle`]). The lock is held only to plan
/// and to store each finished set, never while a pipeline compiles, so a frame drawing
/// effects meanwhile does not wait for the warm.
///
/// # Errors
///
/// A pipeline failed validation, or the context's lock is poisoned.
pub fn warm_shared(gpu: &Mutex<VfxGpu>, formats: &[wgpu::TextureFormat]) -> Result<VfxWarmReport, VfxError> {
    let poisoned = || VfxError::Shader("vfx context lock poisoned".into());
    let plan = gpu.lock().map_err(|_| poisoned())?.warm_plan(formats)?;
    run_plan(plan, |built| {
        if let Ok(mut gpu) = gpu.lock() {
            gpu.store(built);
        }
    })
}

/// A [`regia_render::PipelineWarmQueue`] job that warms `gpu` for `formats` ([`warm_shared`]).
///
/// Queue it once the context exists, typically in [`regia_render::WarmTier::Map`] next to
/// the map's frame jobs, with [`VfxGpu::warm_formats`] of the window format:
///
/// ```ignore
/// queue.push(WarmTier::Map, "vfx", vfx_warm_job(gpu.clone(), VfxGpu::warm_formats(format)));
/// ```
///
/// The job ends with the error `canceled at shutdown` when the queue shut down before it
/// finished.
pub fn vfx_warm_job(gpu: Arc<Mutex<VfxGpu>>, formats: Vec<wgpu::TextureFormat>) -> WarmJob {
    Box::new(move |_device| {
        let report = warm_shared(&gpu, &formats).map_err(|err| err.to_string())?;
        tracing::debug!(
            target: "regia_vfx",
            pipelines = report.pipelines,
            canceled = report.canceled,
            "vfx pipeline warm"
        );
        if report.canceled {
            Err("canceled at shutdown".into())
        } else {
            Ok(())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warm_formats_add_the_hdr_target_once() {
        assert_eq!(
            VfxGpu::warm_formats(wgpu::TextureFormat::Bgra8UnormSrgb),
            vec![
                wgpu::TextureFormat::Bgra8UnormSrgb,
                wgpu::TextureFormat::Rgba16Float
            ]
        );
        assert_eq!(
            VfxGpu::warm_formats(wgpu::TextureFormat::Rgba16Float),
            vec![wgpu::TextureFormat::Rgba16Float]
        );
    }
}
