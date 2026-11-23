// SPDX-License-Identifier: MPL-2.0
//! Render-to-texture views: per-camera settings and the reduced feature set (ADR-184 "Texture
//! views keep their own view state").
//!
//! A camera whose [`crate::RenderTarget`] is `Texture { asset_id }` renders into a persistent
//! texture through [`crate::FrameRenderer::render_texture_view`]. [`TextureViewSettings`] on the
//! same entity sets its size, how often it refreshes and which features it runs; without one
//! the view uses [`TextureViewSettings::default`].

use regia_app::prelude::*;

/// Settings of a render-to-texture camera (`RenderTarget::Texture`): cockpit screens, a target
/// feed, a map.
///
/// The view renders with the reduced feature set ([`Self::apply`]) and keeps its own targets,
/// history and shadow maps, so it never disturbs the window's view. To show it on a surface,
/// bind [`Self::texture`] in a material ([`crate::GraphTexture::from_dynamic`] or
/// [`crate::MaterialImage::Dynamic`]); the renderer copies each new image into that material's
/// texture on the GPU, with no CPU round trip.
///
/// # Example
///
/// ```
/// use regia_render::TextureViewSettings;
///
/// // A 512² screen refreshed at a third of the frame rate, with the image bound on the GPU.
/// let view = TextureViewSettings::with_texture([512, 512]).update_every(3);
/// let screen = regia_render::GraphTexture::from_dynamic(view.texture.clone().unwrap());
/// assert_eq!(screen.dynamic.unwrap().size(), (512, 512));
/// ```
#[derive(Component, Debug, Clone)]
pub struct TextureViewSettings {
    /// Texture size in pixels (each side at least 1). Default [`crate::TEXTURE_TARGET_SIZE`].
    pub size: [u32; 2],
    /// Render on one frame in this many (1 = every frame; 0 counts as 1). A view not rendered
    /// keeps its last image.
    pub update_every: u32,
    /// Render nothing while set; the texture keeps its last image.
    pub paused: bool,
    /// FXAA in the view's post stack (temporal AA never runs in a view).
    pub fxaa: bool,
    /// Bloom in the view's post stack (when the profile has it).
    pub bloom: bool,
    /// Sun shadow cascades (0 = no sun shadows), capped by the profile's. Redrawn whole on
    /// each refresh: a view keeps no paged shadow cache.
    pub sun_cascades: u32,
    /// Shadow-map edge per cascade, px, capped by the profile's.
    pub shadow_resolution: u32,
    /// Point and spot light shadows (a 1024² atlas at most, up to four lights).
    pub local_shadows: bool,
    /// The material texture this view's image is copied into on the GPU (create it at
    /// [`Self::size`], sRGB). `None`: the image is only readable
    /// ([`crate::FrameRenderer::request_texture_readback`]).
    pub texture: Option<crate::DynamicTexture>,
}

impl Default for TextureViewSettings {
    fn default() -> Self {
        Self {
            size: crate::TEXTURE_TARGET_SIZE,
            update_every: 1,
            paused: false,
            fxaa: true,
            bloom: true,
            sun_cascades: 2,
            shadow_resolution: 1024,
            local_shadows: true,
            texture: None,
        }
    }
}

impl TextureViewSettings {
    /// Default settings at `size` with a material texture of that size
    /// ([`Self::texture`]) to bind on surfaces. Each side is clamped to `1..=`[`Self::MAX_EDGE`]
    /// first, the size the view renders at ([`Self::extent`]), so the GPU copy into the
    /// texture (which needs equal sizes) runs.
    #[must_use]
    pub fn with_texture(size: [u32; 2]) -> Self {
        let size = size.map(|side| side.clamp(1, Self::MAX_EDGE));
        Self {
            size,
            texture: Some(crate::DynamicTexture::new(size[0], size[1], true)),
            ..Self::default()
        }
    }

    /// The same settings refreshing on one frame in `frames`.
    #[must_use]
    pub fn update_every(mut self, frames: u32) -> Self {
        self.update_every = frames;
        self
    }

    /// Largest texture side a view renders at; a larger `size` is clamped to it.
    pub const MAX_EDGE: u32 = 16_384;

    /// The texture size, each side between 1 and [`Self::MAX_EDGE`].
    #[must_use]
    pub fn extent(&self) -> (u32, u32) {
        (
            self.size[0].clamp(1, Self::MAX_EDGE),
            self.size[1].clamp(1, Self::MAX_EDGE),
        )
    }

    /// Whether the view renders on frame `frame` of the host (counted by the caller from the
    /// view's first frame): never while paused, else on frames `0, n, 2n, …` for
    /// [`Self::update_every`] `n`.
    #[must_use]
    pub fn renders_on(&self, frame: u64) -> bool {
        !self.paused && frame.is_multiple_of(u64::from(self.update_every.max(1)))
    }

    /// The reduced feature set of a texture view, applied to the frame's quality and post
    /// settings: native scale, no temporal AA or upscale, no SSAO, contact shadows or
    /// reflection occlusion, no SSR or SSGI, no probe update (ambient light), no volumetric fog
    /// or clouds, no screen-space refraction, no visibility buffer (the GPU-driven batches stay
    /// as the profile sets them, frustum-culled with the view's camera and never against the
    /// window's Hi-Z pyramid), at most [`Self::sun_cascades`] cascades of
    /// [`Self::shadow_resolution`] redrawn whole, no near sun tile, no translucent shadow
    /// maps, and in the post stack no temporal AA, motion blur, depth of field, film grain or
    /// SMAA (FXAA and bloom per the settings).
    pub fn apply(&self, quality: &mut crate::FrameQuality, post: Option<&mut crate::PostSettings>) {
        quality.taau = false;
        quality.render_scale_permille = 1000;
        quality.msaa_samples = 1;
        quality.ssao = crate::SsaoMode::Off;
        quality.contact_shadows = false;
        quality.reflection_occlusion = false;
        quality.ssr = false;
        quality.screen_space_gi = false;
        quality.gi_probes = false;
        quality.reflection_probes = false;
        quality.volumetric_fog = false;
        quality.volumetric_clouds = false;
        quality.screen_space_refraction = false;
        quality.visibility_buffer = false;
        quality.depth_velocity = false;
        quality.paged_sun_shadows = false;
        quality.near_sun_tile = 0;
        quality.smaa = false;
        quality.translucent_shadow_cascades = 0;
        quality.translucent_local_shadows = false;
        quality.csm_cascade_count = quality.csm_cascade_count.min(self.sun_cascades.max(1));
        quality.csm_resolution = quality.csm_resolution.min(self.shadow_resolution.max(16));
        if self.local_shadows {
            quality.local_shadow_atlas_size = quality.local_shadow_atlas_size.min(1024);
            quality.local_shadow_max_lights = quality.local_shadow_max_lights.min(4);
        } else {
            quality.local_shadow_atlas_size = 0;
            quality.local_shadow_max_lights = 0;
        }
        if let Some(post) = post {
            post.taa = false;
            post.fxaa = self.fxaa;
            post.bloom &= self.bloom;
            post.film_grain = 0.0;
            post.motion_blur = None;
            post.depth_of_field = None;
        }
    }

    /// Whether the view draws the sun's shadows at all.
    #[must_use]
    pub fn sun_shadows(&self) -> bool {
        self.sun_cascades > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extent_is_clamped_to_the_edge_limit() {
        let view = TextureViewSettings {
            size: [u32::MAX, 0],
            ..TextureViewSettings::default()
        };
        assert_eq!(view.extent(), (TextureViewSettings::MAX_EDGE, 1));
    }

    #[test]
    fn update_every_three_renders_one_frame_in_three() {
        let view = TextureViewSettings::default().update_every(3);
        let rendered: Vec<u64> = (0..9).filter(|&f| view.renders_on(f)).collect();
        assert_eq!(rendered, vec![0, 3, 6]);
        let every = TextureViewSettings::default().update_every(0);
        assert!((0..4).all(|f| every.renders_on(f)), "0 counts as every frame");
        let paused = TextureViewSettings {
            paused: true,
            ..TextureViewSettings::default()
        };
        assert!((0..4).all(|f| !paused.renders_on(f)));
    }

    #[test]
    fn reduced_path_turns_off_history_and_screen_space_work() {
        let mut quality = crate::FrameQuality::from_profile(&crate::RenderProfile::from_preset(
            crate::RenderProfilePreset::DesktopUltra,
        ));
        let mut post = crate::PostSettings {
            taa: true,
            motion_blur: Some(crate::MotionBlur::default()),
            ..crate::PostSettings::default()
        };
        TextureViewSettings::default().apply(&mut quality, Some(&mut post));
        assert!(!quality.taau && !quality.ssr && !quality.screen_space_gi && !quality.gi_probes);
        assert_eq!(quality.ssao, crate::SsaoMode::Off);
        assert!(!quality.paged_sun_shadows && !quality.volumetric_fog && !quality.volumetric_clouds);
        assert!(quality.csm_cascade_count <= 2 && quality.csm_resolution <= 1024);
        assert!(!quality.visibility_buffer);
        // The view batches on the GPU like the window: per-object draws cost a uniform buffer
        // and a bind group at each object's first sight (ADR-184 addendum).
        assert!(quality.gpu_driven, "the profile's GPU-driven path is kept");
        assert!(!post.taa && post.fxaa && post.motion_blur.is_none());
    }

    #[test]
    fn with_texture_matches_the_view_size() {
        let view = TextureViewSettings::with_texture([0, 256]);
        assert_eq!(view.extent(), (1, 256));
        let texture = view.texture.expect("texture");
        assert_eq!(texture.size(), (1, 256));
        assert!(texture.is_srgb());
    }

    /// A side over [`TextureViewSettings::MAX_EDGE`] is clamped for the texture as well as for
    /// the render, so the GPU copy (which needs equal sizes) still runs (audit render10 #7).
    #[test]
    fn with_texture_clamps_an_oversized_side_like_the_view() {
        let view = TextureViewSettings::with_texture([20_000, 512]);
        assert_eq!(view.extent(), (TextureViewSettings::MAX_EDGE, 512));
        assert_eq!(view.size, [TextureViewSettings::MAX_EDGE, 512]);
        let extent = view.extent();
        let texture = view.texture.expect("texture");
        assert_eq!(texture.size(), extent);
    }
}
