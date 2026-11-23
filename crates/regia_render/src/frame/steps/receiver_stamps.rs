// SPDX-License-Identifier: MPL-2.0
//! The decal receiver stamps (ADR-2300): the opaque meshes a decal pass projects onto, built
//! only when a layer has decals (ADR-508). Moved out of `FrameRenderer::render_into` unchanged.

use crate::frame::*;

impl FrameRenderer {
    /// The frame's receiver stamps for the decal pass; counts the stamps a trimmed frame skips.
    pub(in crate::frame) fn receiver_stamps(
        &self,
        draws: &[Draw],
        output_depth: &Option<wgpu::TextureView>,
        trims: bool,
        submit_stats: &mut crate::SubmissionStats,
    ) -> Vec<crate::decal_receiver::Stamp> {
        // Receiver stamps (opaque meshes) for the decal pass, recorded on this encoder
        // before whichever pass draws the decals. Only a decal pass reads them: without a
        // decal in any layer (asked after simulate) none are built (ADR-508).
        let stamps_wanted =
            output_depth.is_some() && (!trims || self.translucent.iter().any(|layer| layer.has_decals()));
        if output_depth.is_some() && !stamps_wanted {
            let skipped = draws
                .iter()
                .filter(|draw| draw.visible && draw.alpha_mode < ALPHA_BLEND)
                .count();
            submit_stats.receiver_stamps_skipped = u32::try_from(skipped).unwrap_or(u32::MAX);
        }
        let mut stamps = Vec::new();
        if stamps_wanted {
            for draw in draws {
                if !draw.visible || draw.alpha_mode >= ALPHA_BLEND {
                    continue;
                }
                let Some(mesh) = self.meshes.get(&draw.mesh) else {
                    continue;
                };
                stamps.push(crate::decal_receiver::Stamp {
                    mesh: draw.mesh,
                    vb: mesh.vb.clone(),
                    ib: mesh.ib.clone(),
                    index_count: mesh.index_count,
                    first_index: mesh.first_index,
                    base_vertex: mesh.base_vertex,
                    model: draw.model,
                    joint_base: draw.joint_base,
                    layer: crate::decal_receiver::stamp_layer(draw.rigid, draw.receiver_layer),
                });
            }
        }
        stamps
    }
}
