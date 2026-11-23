// SPDX-License-Identifier: MPL-2.0
//! The forward shader's frame-level studio axis (ADR-2308).
//!
//! The studio image-light extras of `lab_forward.wgsl` (the ground plane's cover and
//! reflection, the local-capture blend and the image-light debug views) are gated at run time by
//! uniforms, but their code alone cost every frame that never uses them about 0.03-0.05 ms of mesh
//! forward on an RTX 5090 (ADR-2087 (4): a compiler effect of the code being there, not of the
//! branches taken). A frame with no studio feature on ([`crate::StudioFeatures::any`]) now
//! composes the forward module with `STUDIO_IBL_OFF`, the define the Adreno workaround already
//! used (ADR-2042): the per-object forward sets ([`PipelineKey::studio_off`]) and the GPU-driven
//! colour pipelines follow it. Every other forward family (material graphs, mask decals,
//! lightmap receivers, thin film, subsurface, OIT, glass, the visibility-buffer resolve, the lite
//! tiers) keeps the full module, which draws the same pixels in such a frame.
//!
//! The studio-off module draws the full module's pixels in a frame without a studio feature: every
//! block it drops is behind a uniform test that is false there, and the one statement outside
//! such a test (the probe adjustment, a clamp with zero terms) stays. `tests/studio_axis.rs` holds
//! the Day, Exterior and Studio frames byte-identical across the two modules.

use super::*;
use crate::{ForwardStudioMode, ForwardStudioReport, StudioFeatures};

/// [`crate::FORWARD_STUDIO_ENV`] forces the full module in this process (read once).
fn env_forces_full() -> bool {
    static FULL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FULL.get_or_init(|| {
        let value = regia_core::brand::env_var(crate::FORWARD_STUDIO_ENV).ok();
        crate::forward_studio_env_forces_full(value.as_deref())
    })
}

/// Whether the next frame composes the studio-off module, from the renderer's studio state and
/// installed image light (a free function: callers hold the pipeline sets borrowed).
pub(super) fn studio_off_of(studio: &studio_state::StudioState, ibl: Option<&IblGpu>) -> bool {
    if crate::shader_workarounds::sun_shadow_pcf_only() {
        // Every forward module of this process is studio-off already (ADR-2042): one key.
        return true;
    }
    if studio.forward_mode == ForwardStudioMode::Full || env_forces_full() {
        return false;
    }
    !studio.features(ibl).any()
}

impl PipelineKey {
    /// This key with the full forward module: the families that are not on the studio axis
    /// (glass, the lite tiers, lightmap receivers, mask decals) share one cache entry for both.
    pub(super) fn full_studio(self) -> Self {
        Self {
            studio_off: false,
            ..self
        }
    }
}

impl FrameRenderer {
    /// Whether the next frame composes the studio-off forward module (ADR-2308).
    pub(super) fn forward_studio_off(&self) -> bool {
        studio_off_of(&self.studio, self.ibl.as_ref())
    }

    /// How frames pick the forward shader's studio axis ([`ForwardStudioMode`], ADR-2308).
    /// [`ForwardStudioMode::Auto`] (the default) drops the studio image-light extras from the
    /// forward module of a frame that uses none of them; [`ForwardStudioMode::Full`] keeps the
    /// full module on every frame, as before (an A/B baseline). The pixels are the same either
    /// way; a switch builds the other module's pipelines on the next frame (or in the
    /// `forward_studio` warm).
    pub fn set_forward_studio_mode(&mut self, mode: ForwardStudioMode) {
        self.studio.forward_mode = mode;
    }

    /// The mode set with [`Self::set_forward_studio_mode`].
    #[must_use]
    pub fn forward_studio_mode(&self) -> ForwardStudioMode {
        self.studio.forward_mode
    }

    /// The studio features on now: the image light's ground plane (set and fed by the installed
    /// light), local reflection captures, the image-light debug view. Any of them makes a frame
    /// compose the full forward module.
    #[must_use]
    pub fn studio_features(&self) -> StudioFeatures {
        self.studio.features(self.ibl.as_ref())
    }

    /// The forward studio axis: the mode, the features on, the module the next frame composes and
    /// the forward pipelines built for each module (`render.forward_studio`).
    #[must_use]
    pub fn forward_studio_report(&self) -> ForwardStudioReport {
        let (sets_studio_off, sets_full) = self.pipes.as_ref().map_or((0, 0), |pipes| {
            let off = pipes.sets.keys().filter(|key| key.studio_off).count();
            (off, pipes.sets.len() - off)
        });
        let (gpu_color_studio_off, gpu_color_full) = self.culler.color_pipelines_by_studio_axis();
        ForwardStudioReport {
            mode: self.studio.forward_mode,
            env_full: env_forces_full(),
            adreno_workaround: crate::shader_workarounds::sun_shadow_pcf_only(),
            features: self.studio_features(),
            studio_off: self.forward_studio_off(),
            sets_studio_off,
            sets_full,
            gpu_color_studio_off,
            gpu_color_full,
        }
    }

    /// Game-frame warm `forward_studio`: the other studio module of every per-object forward set
    /// the warm drew (with the deferred variants built so far) and of the GPU-driven colour
    /// pipelines, so turning a studio feature on or off mid-play compiles nothing on the frame
    /// path. Nothing unless the renderer has a studio environment installed or a studio feature
    /// on, the mode is [`ForwardStudioMode::Auto`] and the process composes both modules (a game
    /// map without a studio, under [`crate::FORWARD_STUDIO_ENV`] or on Adreno: nothing).
    pub(crate) fn prewarm_forward_studio_twins(
        &mut self,
        device: &RenderDevice,
    ) -> Result<usize, RenderError> {
        let studio_scene = self.studio.installed.is_some() || self.studio_features().any();
        if !studio_scene
            || self.studio.forward_mode != ForwardStudioMode::Auto
            || env_forces_full()
            || crate::shader_workarounds::sun_shadow_pcf_only()
        {
            return Ok(0);
        }
        let d = &device.device;
        let Some(pipes) = self.pipes.as_mut() else {
            return Ok(0);
        };
        let keys: Vec<PipelineKey> = pipes.sets.keys().copied().collect();
        let mut asked = 0;
        for key in &keys {
            crate::pipeline_warm::warm_checkpoint()?;
            let twin = PipelineKey {
                studio_off: !key.studio_off,
                ..*key
            };
            if !pipes.sets.contains_key(&twin) {
                let set = build_pipeline_set(d, pipes, twin);
                pipes.sets.insert(twin, set);
            }
            if let (Some(drawn), Some(set)) = (pipes.sets.get(key), pipes.sets.get(&twin)) {
                for (built, deferred) in deferred_variants(drawn).into_iter().zip(deferred_variants(set)) {
                    if built.is_built() {
                        deferred.get();
                    }
                }
            }
            asked += 1;
        }
        if crate::gpu_driven::IndirectStrategy::select(&device.capabilities).uses_gpu_objects() {
            let (view_bgl, mat_bgl) = (pipes.view_bgl.clone(), pipes.mat_bgl.clone());
            let (frame_sun, frame_studio) = (self.culler.forward_sun_rt(), self.culler.forward_studio_off());
            for key in &keys {
                crate::pipeline_warm::warm_checkpoint()?;
                self.culler.set_forward_sun_rt(key.sun_rt);
                self.culler.set_forward_studio_off(!key.studio_off);
                let built = self.culler.color_pipelines(
                    d,
                    key.format,
                    key.encode,
                    key.samples,
                    true,
                    &view_bgl,
                    &mat_bgl,
                    &[(key.double_sided, false), (key.double_sided, true)],
                );
                // The next frame sets both too, but a failed warm leaves no surprise.
                self.culler.set_forward_sun_rt(frame_sun);
                self.culler.set_forward_studio_off(frame_studio);
                built?;
                asked += 1;
            }
        }
        Ok(asked)
    }
}

/// The deferred variants of a forward pipeline set.
fn deferred_variants(
    set: &PipelineSet,
) -> [&crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>; 9] {
    [
        &set.blend,
        &set.premul,
        &set.add,
        &set.xray,
        &set.transmit_mul,
        &set.transmit_add,
        &set.transmit_add_depth,
        &set.transmit_front,
        &set.transmit_refract,
    ]
}
