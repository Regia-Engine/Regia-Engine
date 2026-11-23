// SPDX-License-Identifier: MPL-2.0
//! Which path shades each decal, and the forward pass's copy of it (ADR-3255).
//!
//! With the forward-decal switch on ([`regia_render::process_forward_decals`]), a decal that writes
//! albedo, normal, roughness or metallic is shaded in the forward pass: the surface takes its
//! inputs before it is lit ([`regia_render::forward_decals`]). The forward shader draws meshes,
//! so the parts of a decal the forward pass cannot reach stay on the projected post pass:
//!
//! - terrain receivers (the terrain shader has no forward-decal gather): a post copy masked to
//!   [`LAYER_TERRAIN`] when the decal's mask has it;
//! - emission (the post pass adds it over the lit colour): a post copy with only
//!   [`CHANNEL_EMISSIVE`] on the mesh layers.
//!
//! A decal marked [`DecalShading::Post`], an emissive-only decal and every decal while the switch
//! is off keep the post path whole. [`route_decals`] returns both lists and one [`DecalPath`] per
//! decal (what `vfx.decal_budget` reports).

use std::sync::{Arc, Mutex};

use regia_render::forward_decals::{
    CHANNEL_METALLIC as FD_METALLIC, FORWARD_CHANNELS, ForwardDecal, ForwardDecalAtlas, ForwardDecalFeed,
};

use crate::decal::{CHANNEL_EMISSIVE, CHANNEL_METALLIC, Decal, DecalShading, DecalSurface, LAYER_TERRAIN};
use crate::decal_atlas::{DECAL_ATLAS_LAYERS, generate_decal_atlas_pages};

/// How one decal is shaded this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecalPath {
    /// The projected post pass only.
    Post,
    /// The forward pass only.
    Forward,
    /// The forward pass on meshes, plus a post copy for terrain receivers and/or emission.
    Split,
}

impl DecalPath {
    /// `"post"`, `"forward"` or `"split"`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Post => "post",
            Self::Forward => "forward",
            Self::Split => "split",
        }
    }
}

/// One frame's decals split between the two paths.
#[derive(Debug, Clone, Default)]
pub struct DecalRoute {
    /// The forward pass's decals (hand to `FrameRenderer::set_forward_decals`).
    pub forward: Vec<ForwardDecal>,
    /// The post pass's boxes (hand to `VfxGpu::write_decals_with` with [`Self::post_surfaces`]).
    pub post: Vec<Decal>,
    /// Surfaces parallel to [`Self::post`].
    pub post_surfaces: Vec<DecalSurface>,
    /// Per input decal, the path it took.
    pub paths: Vec<DecalPath>,
}

impl DecalRoute {
    /// How many decals took each path: `(post, forward, split)`.
    #[must_use]
    pub fn counts(&self) -> (usize, usize, usize) {
        let count = |p| self.paths.iter().filter(|x| **x == p).count();
        (
            count(DecalPath::Post),
            count(DecalPath::Forward),
            count(DecalPath::Split),
        )
    }
}

/// The forward pass's copy of one decal: its box, its surface's material channels (emission
/// left out) and its mask without the terrain bit. The atlas layer reads the forward atlas
/// ([`forward_atlas`]); a layer with a normal page writes the normal from it when the decal has
/// the normal channel. Built-in layers also apply their roughness/metallic multipliers;
/// custom layers supply only color and coverage.
#[must_use]
pub fn forward_copy(decal: &Decal, surface: &DecalSurface) -> ForwardDecal {
    let mut fd = ForwardDecal::from_box(decal.position, decal.rotation, decal.half_size, decal.color);
    fd.albedo = surface.albedo;
    fd.opacity = surface.opacity * decal.fade();
    fd.blend = surface.blend;
    fd.roughness = surface.roughness;
    fd.metallic = surface.metallic;
    fd.normal_xy = surface.normal_xy;
    fd.normal_strength = surface.normal_strength;
    fd.edge_softness = surface.edge_softness;
    fd.angle_fade = surface.angle_fade;
    fd.distance_fade = surface.distance_fade;
    let mut channels = surface.channels & FORWARD_CHANNELS;
    if surface.channels & CHANNEL_METALLIC != 0 {
        channels |= FD_METALLIC;
    }
    fd.channels = channels;
    fd.receiver_mask = forward_mask(surface.receiver_mask);
    let layer = u32::try_from(surface.atlas_layer)
        .ok()
        .filter(|l| *l < regia_render::forward_decals::FORWARD_DECAL_ATLAS_LAYERS);
    fd.atlas_layer = layer;
    fd.normal_page = layer.is_some_and(|l| l < DECAL_ATLAS_LAYERS);
    fd.material_page = layer.is_some_and(|l| l < DECAL_ATLAS_LAYERS);
    fd.priority = surface.priority;
    fd
}

/// A receiver mask without the terrain bit, in the forward shader's terms (`0`: every mesh).
fn forward_mask(mask: u32) -> u32 {
    if mask == 0 || mask == u32::MAX {
        0
    } else {
        mask & !LAYER_TERRAIN
    }
}

/// Whether the post pass still has to draw part of a forward decal: `(terrain, emission)`.
fn post_parts(surface: &DecalSurface) -> (bool, bool) {
    let terrain = surface.receiver_mask == 0
        || surface.receiver_mask == u32::MAX
        || surface.receiver_mask & LAYER_TERRAIN != 0;
    (terrain, surface.channels & CHANNEL_EMISSIVE != 0)
}

/// Splits `decals` (with their parallel `surfaces`; a missing surface uses
/// [`DecalSurface::from_decal`]) between the forward and the post pass. `forward_on` is the
/// forward-decal switch for this frame (the process switch, and a device that has the binding).
#[must_use]
#[tracing::instrument(skip_all, name = "route_decals")]
pub fn route_decals(decals: &[Decal], surfaces: &[DecalSurface], forward_on: bool) -> DecalRoute {
    let mut route = DecalRoute::default();
    for (i, decal) in decals.iter().enumerate() {
        let surface = surfaces
            .get(i)
            .copied()
            .unwrap_or_else(|| DecalSurface::from_decal(decal));
        let forward_channels = surface.channels & (FORWARD_CHANNELS | CHANNEL_METALLIC);
        let forward_mask = forward_mask(surface.receiver_mask);
        let meshes = surface.receiver_mask == 0 || surface.receiver_mask == u32::MAX || forward_mask != 0;
        if !forward_on || surface.shading == DecalShading::Post || forward_channels == 0 || !meshes {
            route.post.push(*decal);
            route.post_surfaces.push(surface);
            route.paths.push(DecalPath::Post);
            continue;
        }
        route.forward.push(forward_copy(decal, &surface));
        let (terrain, emission) = post_parts(&surface);
        if terrain {
            let mut copy = surface;
            copy.receiver_mask = LAYER_TERRAIN;
            route.post.push(*decal);
            route.post_surfaces.push(copy);
        }
        if emission {
            let mut copy = surface;
            copy.channels = CHANNEL_EMISSIVE;
            copy.receiver_mask = if forward_mask == 0 {
                !LAYER_TERRAIN
            } else {
                forward_mask
            };
            route.post.push(*decal);
            route.post_surfaces.push(copy);
        }
        route.paths.push(if terrain || emission {
            DecalPath::Split
        } else {
            DecalPath::Forward
        });
    }
    route
}

/// The runtime's forward-decal hand-off (ADR-3255), shared by the VFX upload system and the
/// runtime's translucent layer: the frame tells the layer whether its device has the forward-decal
/// binding ([`feed_for_frame`]), and the next upload routes the pool's decals by it
/// ([`route_for_upload`]) and publishes the forward copies for the frame to take.
#[derive(Debug, Default)]
pub(crate) struct ForwardFeedState {
    /// What the frame last said about its binding.
    binding: bool,
    /// The last published feed; `None` until an upload routes with the binding on.
    feed: Option<ForwardDecalFeed>,
    /// The built-in forward atlas, made on the first routed upload.
    atlas: Option<Arc<ForwardDecalAtlas>>,
}

/// Updates the atlas snapshot without changing the frame binding handshake.
pub(crate) fn set_texture_atlas(
    shared: &Mutex<ForwardFeedState>,
    atlas: &crate::decal_texture::DecalTextureAtlas,
) {
    if let Ok(mut state) = shared.lock() {
        state.atlas = Some(atlas.forward_atlas());
    }
}

/// What the runtime's layer hands the frame (`TranslucentLayer::forward_decal_feed`): the last
/// published feed. Records `binding` for the next upload.
///
/// A frame without the binding gets `None`, and a published feed that still has decals is
/// replaced by an empty one with the next generation (ADR-7431): the next upload routes every
/// decal to the post pass, so when the frame gets the binding back before an upload routes with
/// it, it takes the empty feed and drops its copy of the old decals instead of shading them a
/// second time over the post pass.
pub(crate) fn feed_for_frame(shared: &Mutex<ForwardFeedState>, binding: bool) -> Option<ForwardDecalFeed> {
    let mut state = shared.lock().ok()?;
    state.binding = binding;
    if !binding {
        if let Some(feed) = state.feed.as_mut()
            && !feed.decals.is_empty()
        {
            feed.generation = feed.generation.wrapping_add(1);
            feed.decals = Arc::from(Vec::new());
        }
        return None;
    }
    state.feed.clone()
}

/// Routes an upload's decals: `None` (the post pass draws them all, as before) while the frame
/// has no forward-decal binding; otherwise the route, whose post list the projected pass draws,
/// with its forward copies published as the next feed.
pub(crate) fn route_for_upload(
    shared: &Mutex<ForwardFeedState>,
    decals: &[Decal],
    surfaces: &[DecalSurface],
) -> Option<DecalRoute> {
    let mut state = shared.lock().ok()?;
    if !state.binding {
        return None;
    }
    let mut route = route_decals(decals, surfaces, true);
    let atlas = Arc::clone(state.atlas.get_or_insert_with(|| Arc::new(forward_atlas())));
    let generation = state.feed.as_ref().map_or(0, |f| f.generation.wrapping_add(1));
    state.feed = Some(ForwardDecalFeed {
        generation,
        decals: std::mem::take(&mut route.forward).into(),
        atlas: Some(atlas),
    });
    Some(route)
}

/// The built-in atlas with its normal and roughness/metallic pages, for
/// `FrameRenderer::set_forward_decal_atlas`.
#[must_use]
pub fn forward_atlas() -> ForwardDecalAtlas {
    ForwardDecalAtlas {
        layers: DECAL_ATLAS_LAYERS,
        rgba: generate_decal_atlas_pages(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decal::{CHANNEL_ALBEDO, CHANNEL_NORMAL, CHANNEL_ROUGHNESS, LAYER_STATIC_MESH, LAYER_WORLD};
    use regia_core::math::{Quat, Vec3};

    fn decal() -> Decal {
        Decal {
            position: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            half_size: Vec3::ONE,
            color: [0.8, 0.1, 0.1, 1.0],
            age: 0.0,
            lifetime: None,
        }
    }

    fn surface(channels: u32, mask: u32) -> DecalSurface {
        let mut s = DecalSurface::from_decal(&decal());
        s.channels = channels;
        s.receiver_mask = mask;
        s
    }

    #[test]
    fn custom_texture_feed_matches_the_projected_snapshot() {
        let mut library = crate::DecalLibrary::builtin();
        let prefab = crate::DecalPrefab::from_ron(
            r#"(version: 3, name: "paint", half_size: (1.0, 1.0, 0.2), texture: Some("paint.fckd"))"#,
        )
        .expect("prefab");
        let image = regia_asset::CookedImage {
            width: 1,
            height: 1,
            rgba8: vec![255, 0, 0, 255],
        };
        library
            .add_textured(prefab.clone(), &image)
            .expect("custom image");
        let shared = Mutex::new(ForwardFeedState::default());
        set_texture_atlas(&shared, library.texture_atlas());
        assert!(feed_for_frame(&shared, true).is_none());
        let (decal, surface) = library
            .get("paint")
            .expect("resolved")
            .instantiate(Vec3::ZERO, Vec3::Y, 0.0, None)
            .expect("place");
        let route = route_for_upload(&shared, &[decal], &[surface]).expect("routed");
        assert_eq!(route.paths, vec![DecalPath::Split]);
        assert_eq!(route.post_surfaces[0].atlas_layer, 4);
        let first = feed_for_frame(&shared, true).expect("feed");
        assert_eq!(first.decals[0].atlas_layer, Some(4));
        let first_atlas = first.atlas.expect("atlas");
        assert!(Arc::ptr_eq(
            &first_atlas,
            &library.texture_atlas().forward_atlas()
        ));
        library
            .add_textured(
                prefab,
                &regia_asset::CookedImage {
                    width: 1,
                    height: 1,
                    rgba8: vec![0, 255, 0, 255],
                },
            )
            .expect("reload image");
        set_texture_atlas(&shared, library.texture_atlas());
        assert!(route_for_upload(&shared, &[decal], &[surface]).is_some());
        let second = feed_for_frame(&shared, true).expect("new feed");
        assert_eq!(second.generation, first.generation + 1);
        let second_atlas = second.atlas.expect("new atlas");
        assert!(!Arc::ptr_eq(&first_atlas, &second_atlas));
        assert!(Arc::ptr_eq(
            &second_atlas,
            &library.texture_atlas().forward_atlas()
        ));
        assert_ne!(first_atlas.rgba, second_atlas.rgba);
        assert_eq!(second.decals[0].atlas_layer, Some(4), "live layer stays stable");
    }

    #[test]
    fn switch_off_keeps_every_decal_on_the_post_pass() {
        let d = [decal(), decal()];
        let s = [
            surface(CHANNEL_ALBEDO | CHANNEL_NORMAL, LAYER_WORLD),
            surface(CHANNEL_ROUGHNESS, 0),
        ];
        let route = route_decals(&d, &s, false);
        assert!(route.forward.is_empty());
        assert_eq!(route.post_surfaces, s.to_vec());
        assert_eq!(route.counts(), (2, 0, 0));
    }

    #[test]
    fn a_world_decal_splits_into_forward_meshes_and_a_terrain_post_copy() {
        let s = [surface(
            CHANNEL_ALBEDO | CHANNEL_NORMAL | CHANNEL_ROUGHNESS,
            LAYER_WORLD,
        )];
        let route = route_decals(&[decal()], &s, true);
        assert_eq!(route.paths, vec![DecalPath::Split]);
        assert_eq!(route.forward.len(), 1);
        assert_eq!(route.forward[0].receiver_mask, LAYER_STATIC_MESH);
        assert_eq!(route.post_surfaces.len(), 1);
        assert_eq!(route.post_surfaces[0].receiver_mask, LAYER_TERRAIN);
    }

    #[test]
    fn a_mesh_only_decal_is_forward_only_and_post_or_emissive_only_stay_post() {
        let mesh = surface(CHANNEL_ALBEDO | CHANNEL_ROUGHNESS, LAYER_STATIC_MESH);
        let mut marked = mesh;
        marked.shading = DecalShading::Post;
        let glow = surface(CHANNEL_EMISSIVE, LAYER_STATIC_MESH);
        let terrain_only = surface(CHANNEL_ALBEDO, LAYER_TERRAIN);
        let route = route_decals(&[decal(); 4], &[mesh, marked, glow, terrain_only], true);
        assert_eq!(
            route.paths,
            vec![
                DecalPath::Forward,
                DecalPath::Post,
                DecalPath::Post,
                DecalPath::Post
            ]
        );
        assert_eq!(route.forward.len(), 1);
        assert_eq!(route.post.len(), 3);
    }

    #[test]
    fn an_emissive_material_decal_leaves_only_its_emission_on_meshes_to_the_post_pass() {
        let s = surface(CHANNEL_ALBEDO | CHANNEL_EMISSIVE, LAYER_STATIC_MESH);
        let route = route_decals(&[decal()], &[s], true);
        assert_eq!(route.paths, vec![DecalPath::Split]);
        assert_eq!(route.forward[0].channels, CHANNEL_ALBEDO);
        assert_eq!(route.post_surfaces[0].channels, CHANNEL_EMISSIVE);
        assert_eq!(route.post_surfaces[0].receiver_mask, LAYER_STATIC_MESH);
    }

    #[test]
    fn the_forward_copy_carries_fade_metallic_and_the_atlas_pages() {
        let mut d = decal();
        d.lifetime = Some(1.0);
        d.age = 0.95;
        let mut s = surface(CHANNEL_ALBEDO | CHANNEL_METALLIC, LAYER_STATIC_MESH);
        s.metallic = 0.6;
        s.atlas_layer = 1;
        let fd = forward_copy(&d, &s);
        assert!((fd.opacity - d.fade()).abs() < 1e-6);
        assert!(fd.opacity < 1.0);
        assert_eq!(fd.channels, CHANNEL_ALBEDO | FD_METALLIC);
        assert!((fd.metallic - 0.6).abs() < 1e-6);
        assert_eq!(fd.atlas_layer, Some(1));
        assert!(fd.normal_page && fd.material_page);
        let atlas = forward_atlas();
        assert!(atlas.validate().is_ok());
    }

    #[test]
    fn the_runtime_routes_by_the_frame_binding_and_publishes_a_feed_per_upload() {
        let shared = Mutex::new(ForwardFeedState::default());
        let d = [decal(), decal()];
        let s = [
            surface(CHANNEL_ALBEDO | CHANNEL_ROUGHNESS, LAYER_STATIC_MESH),
            surface(CHANNEL_EMISSIVE, LAYER_STATIC_MESH),
        ];
        // Before the frame says anything (and while it has no binding): the post pass draws all
        // of them and the layer hands the frame nothing.
        assert!(route_for_upload(&shared, &d, &s).is_none());
        assert!(feed_for_frame(&shared, false).is_none());
        assert!(route_for_upload(&shared, &d, &s).is_none());
        // The frame has the binding: the next upload routes and publishes.
        assert!(feed_for_frame(&shared, true).is_none(), "nothing published yet");
        let route = route_for_upload(&shared, &d, &s).expect("routed");
        assert_eq!(route.paths, vec![DecalPath::Forward, DecalPath::Post]);
        assert!(route.forward.is_empty(), "the forward copies moved into the feed");
        assert_eq!(route.post_surfaces, vec![s[1]]);
        let first = feed_for_frame(&shared, true).expect("a feed");
        assert_eq!(first.generation, 0);
        assert_eq!(first.decals.len(), 1);
        let atlas = first.atlas.expect("the built-in atlas");
        assert!(atlas.validate().is_ok());
        // Every upload bumps the generation (decals fade as they age); the atlas is made once.
        assert!(route_for_upload(&shared, &d, &s).is_some());
        let second = feed_for_frame(&shared, true).expect("the last feed");
        assert_eq!(second.generation, 1);
        assert!(Arc::ptr_eq(&atlas, &second.atlas.expect("atlas")));
        // The frame lost the binding: it is handed nothing (ADR-7431), and the post pass draws
        // every decal again.
        assert!(feed_for_frame(&shared, false).is_none());
        assert!(route_for_upload(&shared, &d, &s).is_none());
    }

    /// ADR-7431 (2): a frame's shading of one mesh-only decal over a run where the device loses
    /// the forward-decal binding and gets it back. Each frame is an upload (routed by what the last
    /// frame said) then the frame: it takes the layer's feed as `ForwardDecalHost::take_feed` does
    /// (the decals are copied when the generation moved, kept otherwise) and shades the copy in the
    /// forward pass only while it has the binding. Post draws plus forward draws must never be two:
    /// before ADR-7431 the frame that got the binding back shaded the feed published before it lost
    /// it while the post pass, routed without the binding, drew the same decal (double-drawn).
    #[test]
    fn a_frame_that_gets_the_binding_back_never_shades_a_stale_feed() {
        let shared = Mutex::new(ForwardFeedState::default());
        let d = [decal()];
        let s = [surface(CHANNEL_ALBEDO | CHANNEL_ROUGHNESS, LAYER_STATIC_MESH)];
        // The frame side: the generation taken last and the decals it keeps.
        let mut taken: Option<u64> = None;
        let mut kept = 0usize;
        let bindings = [true, true, true, false, false, true, true, false, true];
        let mut draws = Vec::new();
        for (frame, &binding) in bindings.iter().enumerate() {
            let post = route_for_upload(&shared, &d, &s).map_or(d.len(), |r| r.post.len());
            if let Some(feed) = feed_for_frame(&shared, binding)
                && taken != Some(feed.generation)
            {
                taken = Some(feed.generation);
                kept = feed.decals.len();
            }
            let forward = if binding { kept } else { 0 };
            draws.push(post + forward);
            assert!(
                post + forward <= 1,
                "frame {frame} (binding {binding}): post {post} + forward {forward}: double-drawn"
            );
        }
        // Frame 0 routes before the frame first said it has the binding (post); a frame that loses
        // the binding was routed forward by the frame before, so it has a one-frame gap (the
        // upload runs before the frame says so); every other frame shades the decal once.
        assert_eq!(draws, vec![1, 1, 1, 0, 1, 1, 1, 0, 1]);
    }
}
