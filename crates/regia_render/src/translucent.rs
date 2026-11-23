// SPDX-License-Identifier: MPL-2.0
//! The translucent-layer hook: external effect passes drawn by the frame renderer.
//!
//! This is the integration seam for `regia_vfx` (spec `26` § 3: particles, decals, trails
//! are simulated in compute and drawn after opaques). Implementors own every GPU resource;
//! the frame renderer calls them twice per frame:
//!
//! 1. [`TranslucentLayer::simulate`] on the frame's command encoder, after the forward pass
//!    is recorded (the scene depth view is passed for depth collision and soft particles);
//! 2. [`TranslucentLayer::draw`] inside a final colour pass — after the post stack when it is
//!    enabled — whose colour attachment is the frame's target and whose depth attachment is
//!    the scene depth, **read-only** (`depth_ops: None`), because effect shaders sample it.
//!    When the scene was rendered below the output size, that attachment is a nearest copy
//!    of the scene depth at the output size (the two attachments must match).
//!
//! Layers that do not participate in HDR draw in display space (the 8-bit target the host
//! presents). A layer that does ([`TranslucentLayer::participates_in_hdr`]) is drawn into the
//! pre-tonemap `Rgba16Float` target when the frame is single-sample, so its emissive cores
//! feed bloom. That path is skipped for MSAA and `Rg11b10` (no alpha); those frames use the
//! display pass. See ADR-049.
//!
//! On an HDR frame ([`LayerCamera::hdr`]) everything such a layer draws — decals first, then
//! [`TranslucentLayer::draw_hdr`] — lands in the pre-exposure target, before the post stack
//! multiplies by [`LayerCamera::exposure`]. The decals draw in their own pass; the scene
//! copy bound by [`TranslucentLayer::bind_decal_targets`] is then refreshed (it holds the
//! frame after decals) before the `draw_hdr` pass, so a refraction effect can sample it. Every colour the layer writes there must be
//! divided by that exposure, or it prints black at daylight EV (2.5e-5 at EV 15). The layer
//! then draws nothing on the display pass. An MSAA frame resolves its depth to one sample
//! for the display pass.

/// The camera contract passed to [`TranslucentLayer::simulate`] each frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerCamera {
    /// World-space eye.
    pub eye: [f32; 3],
    /// World-space look-at point.
    pub target: [f32; 3],
    /// Vertical field of view (radians).
    pub fov_y: f32,
    /// Near plane.
    pub near: f32,
    /// Far plane.
    pub far: f32,
    /// Viewport width in pixels.
    pub width: u32,
    /// Viewport height in pixels.
    pub height: u32,
    /// Exposure multiplier the post stack applies this frame (manual EV, physical camera or
    /// the auto-exposure meter), or 1 when no post stack runs. HDR draws divide their colour
    /// by this so the authored value survives the later multiply.
    pub exposure: f32,
    /// The post stack's own exposure multiplier (scene radiance → display), or 1 when no post
    /// stack runs. Equals [`Self::exposure`] unless the HDR target is pre-exposed, where
    /// `exposure` is post ÷ pre-exposure. Layers map a light's lux to display brightness with
    /// this (`illuminance / π × display_exposure` is how bright a white surface shows).
    pub display_exposure: f32,
    /// `true` when this frame draws participating layers into the pre-exposure HDR target
    /// ([`TranslucentLayer::draw_decals`] and [`TranslucentLayer::draw_hdr`]). `false`: the
    /// display pass, where colours are display-referred and must not be divided.
    pub hdr: bool,
    /// Unit world direction toward the frame's sun (the directional light the meshes and
    /// the sky use this frame, after any atmosphere override). Decal relight and glint and
    /// particle lighting follow it.
    pub sun_direction: [f32; 3],
    /// Linear colour of that sun.
    pub sun_color: [f32; 3],
    /// Its illuminance in lux (`0` = no sun this frame).
    pub sun_illuminance: f32,
    /// The sky light at the ground this frame as radiance (`E / π`, cd/m²): `[sky from above,
    /// ground bounce below]`, the hemisphere ambient terrain and meshes are lit with. `None`
    /// without an atmosphere. Lit particles take it (ADR-1425).
    pub sky_ambient: Option<[[f32; 3]; 2]>,
}

/// A translucent effect layer (VFX particles, projected decals, trails) drawn after the
/// frame's colour passes.
///
/// Implementations must be cheap to clone behind an `Arc` (the renderer keeps them for its
/// lifetime) and must not panic: a layer that cannot record its work records nothing.
pub trait TranslucentLayer: Send + Sync {
    /// Records GPU simulation work (compute) onto the frame encoder, once per frame before
    /// the colour passes are submitted.
    ///
    /// `depth` is the frame's scene depth (`Depth32Float`) — bind it for depth collision and
    /// soft-particle fading.
    fn simulate(&self, depth: &wgpu::TextureView, camera: &LayerCamera, encoder: &mut wgpu::CommandEncoder);

    /// Draws into the final colour pass: the colour attachment is the frame target in
    /// `format` (8-bit sRGB or Unorm), the depth attachment is the scene depth, read-only.
    /// Layers that cannot draw into `format` record nothing.
    fn draw(&self, pass: &mut wgpu::RenderPass<'_>, format: wgpu::TextureFormat);

    /// `true` when [`Self::draw_hdr`] should run into the pre-tonemap HDR target on a
    /// single-sample `Rgba16Float` frame. The default stays on the display pass only.
    fn participates_in_hdr(&self) -> bool {
        false
    }

    /// `false` when the layer has nothing to simulate or draw this frame (no live effect,
    /// decal, trail or weather). When no installed layer is active the frame leaves them out
    /// entirely, as if none were installed: no [`Self::simulate`] call, no decal receiver or
    /// scene-colour copy, no HDR copy and no pass (ADR-2585). Asked once per frame, before
    /// [`Self::simulate`]; a layer that goes active again must draw with the targets and
    /// pipelines it already built. Default `true`.
    fn is_active(&self) -> bool {
        true
    }

    /// Draws into the pre-tonemap HDR colour target (`format` is `Rgba16Float`). Depth is
    /// the scene depth, read-only. The default records nothing.
    fn draw_hdr(&self, _pass: &mut wgpu::RenderPass<'_>, _format: wgpu::TextureFormat) {}

    /// Draws projected decals into the same colour pass, before [`TranslucentLayer::draw`]
    /// (or before [`TranslucentLayer::draw_hdr`] on an HDR frame, in the HDR pass, so a scorch
    /// sits under the fire and smoke in front of it).
    ///
    /// The default records nothing. Depth is read-only, so a host without a G-buffer still
    /// projects them. See the decal ADR in `docs/decisions.md`.
    fn draw_decals(&self, _pass: &mut wgpu::RenderPass<'_>, _format: wgpu::TextureFormat) {}

    /// Binds the decal receiver and an optional scene-colour copy before
    /// [`TranslucentLayer::simulate`].
    ///
    /// The frame renderer calls this once the views exist. The receiver may still be
    /// cleared, and the copy may still be filled, later in the frame — before
    /// [`TranslucentLayer::draw_decals`]. The default ignores both views.
    fn bind_decal_targets(&self, _receiver: Option<&wgpu::TextureView>, _scene: Option<&wgpu::TextureView>) {}

    /// `true` when the layer samples the scene copy after its decals (a refraction effect).
    /// On an HDR frame the renderer then re-copies the buffer between the decal and the
    /// `draw_hdr` passes; otherwise it skips that full-screen copy. Default `false`.
    fn wants_scene_after_decals(&self) -> bool {
        false
    }

    /// `true` when the layer will draw decals this frame. The renderer skips the receiver
    /// stamp and the scene-colour copy when no layer does. Default `true`.
    fn has_decals(&self) -> bool {
        true
    }

    /// The occlusion view this layer wants this frame (spec 99 § 5.6). Default `None`.
    fn precipitation_occlusion_request(&self) -> Option<crate::surface_weather::PrecipOcclusionRequest> {
        None
    }

    /// The occlusion view aimed along the frame's sun, if this layer wants it (effects that show
    /// only in the sun's shafts). It is the lowest claim on the tile: the view stays with rain,
    /// snow and a host's request, and a wet or snowy surface keeps it straight down, so it is drawn
    /// only on a dry frame nothing else needs it on. It arrives through
    /// [`TranslucentLayer::bind_sun_occlusion`]. Default `None`.
    fn sun_occlusion_request(&self) -> Option<crate::surface_weather::SunOcclusionRequest> {
        None
    }

    /// Binds the occlusion view aimed along the sun ([`TranslucentLayer::sun_occlusion_request`])
    /// before [`TranslucentLayer::simulate`]; `None` when the frame has none.
    /// [`TranslucentLayer::bind_precipitation_occlusion`] never receives this view. The default
    /// ignores it.
    fn bind_sun_occlusion(&self, _binding: Option<&crate::surface_weather::PrecipOcclusionBinding<'_>>) {}

    /// Binds the precipitation occlusion view before [`TranslucentLayer::simulate`]; `None`
    /// when the frame has none. The default ignores it.
    fn bind_precipitation_occlusion(
        &self,
        _binding: Option<&crate::surface_weather::PrecipOcclusionBinding<'_>>,
    ) {
    }

    /// Tells the layer whether `format` stores linear values (`Rgba8UnormSrgb`),
    /// display-encoded bytes (`Rgba8Unorm`) or pre-exposure radiance (`Rgba16Float` on an
    /// HDR frame).
    ///
    /// Called after [`TranslucentLayer::simulate`] and before the colour pass, on the
    /// same frame the decals composite. The default ignores it.
    fn note_colour_format(&self, _format: wgpu::TextureFormat) {}

    /// The forward decals this layer hands the frame (ADR-3255), asked before each view's forward
    /// pass is staged. `binding` tells the layer whether the renderer's device has the
    /// forward-decal binding (the process switch on a device with room): a layer routes its next
    /// frame's decals by it, and keeps every decal on its projected pass without it. A feed whose
    /// generation moved replaces the decals the host set with `FrameRenderer::set_forward_decals`.
    /// The default supplies none.
    fn forward_decal_feed(&self, _binding: bool) -> Option<crate::forward_decals::ForwardDecalFeed> {
        None
    }

    /// The outdoors the next view sees from inside a cell ([`crate::viscells::OutdoorPortals`],
    /// ADR-8843; [`crate::FrameRenderer::set_outdoor_portals`]). A layer of outdoor content (the
    /// water) may leave out what lies in none of the list's frusta on a frame whose
    /// [`LayerCamera::eye`] the list [restricts](crate::viscells::OutdoorPortals::restricts).
    /// Kept until the next call. The default ignores it.
    fn set_outdoor_portals(&self, _portals: Option<&crate::viscells::OutdoorPortals>) {}
}
