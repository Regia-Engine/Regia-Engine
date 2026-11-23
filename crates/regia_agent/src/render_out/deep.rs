// SPDX-License-Identifier: MPL-2.0
//! The render-out's 16-bit offscreen render (`bit_depth: 16`, ADR-1404): the extracted frame
//! drawn into the job's own `Rgba16Float` target ([`regia_render::DeepTarget`]) and read back as
//! 16-bit sRGB codes. The same checks and scene setup as the 8-bit offscreen capture
//! (`crate::seeing::offscreen_pixels`), without window overlays or UI layers.

use regia_app::prelude::*;
use regia_render::DeepTarget;
use regia_render::runtime::{ExtractedFrame, RuntimeRenderer};

use crate::protocol::{self, RpcError};

fn nothing_to_render() -> RpcError {
    RpcError::new(
        protocol::UNSUPPORTED,
        "F0011",
        "nothing to capture offscreen",
        "spawn a Camera and a MeshRenderer; offscreen capture renders the extracted frame",
    )
}

/// Renders the extracted frame at `width`×`height` into `target` (created, or re-created at a new
/// size) and returns its RGBA16 codes.
pub(crate) fn offscreen_pixels_deep(
    world: &mut World,
    width: u32,
    height: u32,
    target: &mut Option<DeepTarget>,
) -> Result<Vec<u16>, RpcError> {
    if world.get_resource::<regia_render::RenderDevice>().is_none() {
        if let Some(disabled) = world.get_resource::<crate::seeing::GpuDisabled>() {
            return Err(disabled.error());
        }
        let device = crate::seeing::headless_render_device().map_err(|e| {
            RpcError::new(
                protocol::UNSUPPORTED,
                "F0011",
                format!("no GPU for offscreen capture: {e}"),
                "install lavapipe (Vulkan software) or a GPU driver",
            )
        })?;
        world.insert_resource(device);
    }
    if world.get_resource::<RuntimeRenderer>().is_none() {
        return Err(RpcError::new(
            protocol::UNSUPPORTED,
            "F0011",
            "no RuntimeRenderer",
            "add regia_render::RenderPlugin { headless: true }",
        ));
    }
    world.resource_scope(|world, mut runtime: Mut<RuntimeRenderer>| {
        let device = world.resource::<regia_render::RenderDevice>();
        let extracted = world
            .get_resource::<ExtractedFrame>()
            .filter(|frame| frame.is_renderable())
            .ok_or_else(nothing_to_render)?;
        let view_index = extracted
            .window_views()
            .first()
            .map(|(index, _)| *index)
            .unwrap_or(0);
        runtime.0.set_glass(extracted.glass.clone());
        runtime.0.set_valley_mist(extracted.valley_mist.clone());
        runtime.0.set_mask_decals(&extracted.mask_decals);
        runtime.0.set_morph_instances(&extracted.morphs);
        let scene = extracted.frame_scene(view_index, width, height);
        let deep = match target.take() {
            Some(existing) if existing.fits(scene.width, scene.height) => existing,
            _ => DeepTarget::new(device, scene.width, scene.height),
        };
        let frame = runtime.0.render_offscreen_deep(device, &scene, &deep);
        *target = Some(deep);
        frame
            .map(|frame| frame.rgba16)
            .map_err(|e| RpcError::internal(format!("offscreen render failed: {e}")))
    })
}
