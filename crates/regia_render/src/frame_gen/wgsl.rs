// SPDX-License-Identifier: MPL-2.0
//! The WGSL of the frame generation passes (ADR-2067). Ported from the AMD FidelityFX SDK v1.1.4
//! (MIT; the notice is in each file under `shaders/`).

/// Uniform and helpers of the interpolation passes.
pub(crate) const COMMON: &str = include_str!("shaders/fg_common.wgsl");
/// The game vector field reader with its inpainting.
pub(crate) const FIELD_LOAD: &str = include_str!("shaders/fg_field_load.wgsl");
/// Optical flow: the luma pyramid and the scene-change test.
pub(crate) const FLOW_LUMA: &str = include_str!("shaders/fg_flow_luma.wgsl");
/// Optical flow: search, filter, upscale.
pub(crate) const FLOW: &str = include_str!("shaders/fg_flow.wgsl");
/// Prepare inputs: dilated depth and motion, reconstructed previous depth.
pub(crate) const PREPARE: &str = include_str!("shaders/fg_prepare.wgsl");
/// The depth estimate of the interpolated frame.
pub(crate) const RECONSTRUCT_DEPTH: &str = include_str!("shaders/fg_reconstruct_depth.wgsl");
/// The game motion vector field.
pub(crate) const GAME_FIELD: &str = include_str!("shaders/fg_game_field.wgsl");
/// The optical flow vector field.
pub(crate) const FLOW_FIELD: &str = include_str!("shaders/fg_flow_field.wgsl");
/// The disocclusion mask.
pub(crate) const DISOCCLUSION: &str = include_str!("shaders/fg_disocclusion.wgsl");
/// The inpainting pyramid.
pub(crate) const PYRAMID: &str = include_str!("shaders/fg_pyramid.wgsl");
/// The interpolated colour.
pub(crate) const INTERPOLATE: &str = include_str!("shaders/fg_interpolate.wgsl");
/// Inpainting and the debug bars.
pub(crate) const INPAINT: &str = include_str!("shaders/fg_inpaint.wgsl");
/// The debug view.
pub(crate) const DEBUG_VIEW: &str = include_str!("shaders/fg_debug.wgsl");
/// The copy onto the present target.
pub(crate) const BLIT: &str = include_str!("shaders/fg_blit.wgsl");

/// The source of a pass with the shared modules it needs.
pub(crate) fn with_common(pass: &str) -> String {
    format!("{COMMON}\n{pass}")
}

/// The source of a pass that also reads the game vector field.
pub(crate) fn with_field(pass: &str) -> String {
    format!("{COMMON}\n{FIELD_LOAD}\n{pass}")
}

/// Every module as the renderer composes it, `(file name, source)`, for the shader gate.
pub(crate) fn checked_sources() -> Vec<(&'static str, &'static str)> {
    static SOURCES: std::sync::OnceLock<Vec<(&'static str, String)>> = std::sync::OnceLock::new();
    SOURCES
        .get_or_init(|| {
            vec![
                ("fg_flow_luma.wgsl", FLOW_LUMA.to_owned()),
                ("fg_flow.wgsl", FLOW.to_owned()),
                ("fg_prepare.wgsl", with_common(PREPARE)),
                ("fg_reconstruct_depth.wgsl", with_common(RECONSTRUCT_DEPTH)),
                ("fg_game_field.wgsl", with_common(GAME_FIELD)),
                ("fg_flow_field.wgsl", with_common(FLOW_FIELD)),
                ("fg_disocclusion.wgsl", with_field(DISOCCLUSION)),
                ("fg_pyramid.wgsl", with_common(PYRAMID)),
                ("fg_interpolate.wgsl", with_field(INTERPOLATE)),
                ("fg_inpaint.wgsl", with_common(INPAINT)),
                ("fg_debug.wgsl", with_common(DEBUG_VIEW)),
                ("fg_blit.wgsl", BLIT.to_owned()),
            ]
        })
        .iter()
        .map(|(name, source)| (*name, source.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_module_composes_and_validates() {
        for (name, source) in checked_sources() {
            crate::shader_lib::compose_source(source, name, std::collections::HashMap::new())
                .unwrap_or_else(|err| panic!("{name}: {err}"));
        }
    }
}
