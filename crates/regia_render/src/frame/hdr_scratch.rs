// SPDX-License-Identifier: MPL-2.0
//! The two HDR scratch images as transients of the view's pool (ADR-7877, ledger M03 part 1).
//!
//! When the frame's composite is one a later frame reuses (the static-screen cache, the TAAU
//! resolve), the translucent layers and the outdoor composite draw into a copy of it, so the
//! reused image keeps no effects: slot 0, `translucent hdr scratch`, and slot 1, `outdoor hdr
//! scratch`. Both have one description at one size and format, so each is acquired under its own
//! slot key ([`crate::TransientPool::acquire_keyed`]) and the pool never hands one the other's
//! texture.
//!
//! They live in the view's keyed pool (`FrameRenderer::keyed_transients`, swapped with the view
//! state like the frame pool), not in the frame pool: the frame pool's counters
//! (`FrameGraphInfo::transients`) keep describing the frame transients, and
//! `FrameGraphInfo::keyed` counts these.
//!
//! Lifetime. Post builds its bind groups for both views ahead of the first held frame (ADR-1361),
//! so a scratch image must outlive the frames that do not draw into it. Every pool frame holds
//! the latest texture of each slot ([`crate::TransientPool::keep_latest`]): it lives as long as
//! the renderer field it replaces did, except that an older one (another size or format) is
//! dropped after [`crate::TransientPool::DEFAULT_IDLE_FRAMES`] pool frames instead of at once.
//! A drop renews the bind epoch (`steps::begin`), so no cached group, post's included, keeps
//! naming a texture the pool let go.

use crate::TrackedDevice;

use super::FrameRenderer;

/// Usage of both scratch images (a copy destination, an attachment the effects draw into, a
/// texture post samples, a copy source for the scene copies taken after the decals).
pub(super) const HDR_SCRATCH_USAGE: wgpu::TextureUsages = wgpu::TextureUsages::RENDER_ATTACHMENT
    .union(wgpu::TextureUsages::TEXTURE_BINDING)
    .union(wgpu::TextureUsages::COPY_DST)
    .union(wgpu::TextureUsages::COPY_SRC);

/// One of the two scratch images.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HdrScratch {
    /// Slot 0: the translucent layers' copy (`translucent_hdr`).
    Translucent,
    /// Slot 1: the outdoor composite's copy (`atmosphere`, `clouds`, `fog`).
    Outdoor,
}

impl HdrScratch {
    /// Both, in slot order.
    pub(super) const ALL: [Self; 2] = [Self::Translucent, Self::Outdoor];

    /// The slot key in the view's transient pool (never 0, which is unkeyed).
    pub(super) const fn key(self) -> u32 {
        match self {
            Self::Translucent => 1,
            Self::Outdoor => 2,
        }
    }

    /// The texture label (unchanged from the renderer fields).
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Translucent => "translucent hdr scratch",
            Self::Outdoor => "outdoor hdr scratch",
        }
    }

    /// The frame graph resource its writers declare as a transient.
    pub(super) const fn resource(self) -> &'static str {
        match self {
            Self::Translucent => "translucent_hdr_scratch",
            Self::Outdoor => "outdoor_hdr_scratch",
        }
    }
}

impl FrameRenderer {
    /// The scratch image `slot` at `width` × `height` in `format`: a keyed transient of the view's
    /// pool, the same texture every frame at one size and format.
    pub(super) fn hdr_scratch(
        &mut self,
        slot: HdrScratch,
        d: &TrackedDevice,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> crate::TransientTexture {
        let desc = crate::TextureDesc::new(width, height, format).with_usage(HDR_SCRATCH_USAGE);
        self.keyed_transients
            .acquire_keyed(d, &desc, slot.key(), slot.label())
    }

    /// Starts the keyed pool's frame `now` and holds the latest texture of each scratch slot
    /// through it (see the module docs). Called once per `render_into`, with the frame pool's
    /// frame. Returns `true` when the pool dropped a texture (the caller renews the bind epoch).
    pub(super) fn begin_hdr_scratch_frame(&mut self, now: u64) -> bool {
        let dropped = self.keyed_transients.begin_frame(now);
        for slot in HdrScratch::ALL {
            self.keyed_transients.keep_latest(slot.key());
        }
        dropped
    }

    /// The view of the latest scratch image of `slot`, if the view's pool holds one (post's
    /// prewarm candidates, ADR-1361).
    pub(super) fn hdr_scratch_view(&self, slot: HdrScratch) -> Option<wgpu::TextureView> {
        self.keyed_transients
            .latest(slot.key())
            .map(|texture| texture.view.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex, OutdoorParams, PostSettings,
        RenderProfile, RenderProfilePreset, TimeOfDay, TransientEntryInfo, VolumetricFogSettings,
    };
    use std::collections::BTreeMap;

    /// A translucent layer that takes part in the HDR pass and draws nothing: it makes the frame
    /// record `translucent_hdr` (and so take the translucent scratch while the screen is keyed).
    struct HdrLayer;

    impl crate::translucent::TranslucentLayer for HdrLayer {
        fn simulate(
            &self,
            _depth: &wgpu::TextureView,
            _camera: &crate::translucent::LayerCamera,
            _encoder: &mut wgpu::CommandEncoder,
        ) {
        }

        fn draw(&self, _pass: &mut wgpu::RenderPass<'_>, _format: wgpu::TextureFormat) {}

        fn participates_in_hdr(&self) -> bool {
            true
        }
    }

    fn quad(corners: [[f32; 3]; 4], normal: [f32; 3]) -> LabMesh {
        LabMesh {
            vertices: corners
                .iter()
                .zip([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
                .map(|(p, uv)| LabVertex::from_pos_normal_uv(*p, normal, uv))
                .collect(),
            indices: vec![0, 1, 2, 0, 2, 3],
            material: 0,
            ..LabMesh::unskinned()
        }
    }

    /// An outdoor ground and wall under haze (the held-haze scene), `width` × `height`.
    fn scene(width: u32, height: u32) -> LabScene {
        LabScene {
            width,
            height,
            eye: [0.0, 1.8, 0.0],
            target: [0.0, 6.0, -60.0],
            fov_y: 0.9,
            near: 0.1,
            far: 2_000.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: true,
            exposure_ev: 14.0,
            ambient_lux: 20_000.0,
            double_sided: false,
            post: None,
            directional: crate::DirectionalLightUniform::new([0.3, -0.6, -0.4], [1.0, 0.95, 0.9], 100_000.0),
            local_lights: Vec::new(),
            materials: vec![LabMaterial {
                base_color: [0.4, 0.3, 0.25, 1.0],
                roughness: 0.9,
                ..LabMaterial::default()
            }],
            meshes: vec![
                quad(
                    [
                        [-200.0, 0.0, 20.0],
                        [200.0, 0.0, 20.0],
                        [200.0, 0.0, -200.0],
                        [-200.0, 0.0, -200.0],
                    ],
                    [0.0, 1.0, 0.0],
                ),
                quad(
                    [
                        [-150.0, 0.0, -120.0],
                        [150.0, 0.0, -120.0],
                        [150.0, 60.0, -120.0],
                        [-150.0, 60.0, -120.0],
                    ],
                    [0.0, 0.0, 1.0],
                ),
            ],
        }
    }

    fn renderer() -> FrameRenderer {
        let mut renderer = FrameRenderer::new();
        renderer.set_outdoor(OutdoorParams {
            time: Some(TimeOfDay::noon()),
            fog: VolumetricFogSettings {
                density: 0.01,
                height_falloff: 0.05,
                ..VolumetricFogSettings::default()
            },
            ..OutdoorParams::default()
        });
        renderer.set_translucent_layers(vec![std::sync::Arc::new(HdrLayer)]);
        renderer
    }

    fn post() -> PostSettings {
        PostSettings {
            auto_exposure: true,
            exposure_ev: 14.0,
            bloom: false,
            ..PostSettings::golden_deterministic()
        }
    }

    fn quality() -> FrameQuality {
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh))
    }

    fn scratch_entries(renderer: &FrameRenderer) -> Vec<TransientEntryInfo> {
        renderer
            .keyed_transients
            .entry_info()
            .into_iter()
            .filter(|entry| entry.key != 0)
            .collect()
    }

    /// Both scratch images are keyed transients of the view's keyed pool, under two keys and
    /// their own labels, counted in `graph_report().graph.keyed` (not in the frame pool's
    /// `transients`) and listed in `pooled`; a steady frame takes the same two textures and
    /// creates none.
    #[test]
    fn both_hdr_scratch_images_are_keyed_transients_of_the_views_pool() {
        let device = crate::test_device();
        let quality = quality();
        assert!(
            quality.reuse_static_screen(),
            "the scratch copies need the static-screen key"
        );
        let post = post();
        let lab = scene(96, 64);
        let mut frame = lab.as_frame_scene_with_quality(quality);
        frame.post = Some(&post);
        let mut renderer = renderer();
        for _ in 0..3 {
            renderer.render_offscreen(&device, &frame).expect("frame");
        }
        let entries = scratch_entries(&renderer);
        let labels: BTreeMap<u32, &str> = entries.iter().map(|e| (e.key, e.label.as_str())).collect();
        assert_eq!(
            labels,
            BTreeMap::from([
                (HdrScratch::Translucent.key(), "translucent hdr scratch"),
                (HdrScratch::Outdoor.key(), "outdoor hdr scratch"),
            ]),
            "{entries:?}"
        );
        assert_eq!(entries.len(), 2, "one texture per slot at one size: {entries:?}");
        assert_ne!(entries[0].generation, entries[1].generation);
        for entry in &entries {
            assert_eq!(entry.desc.usage, HDR_SCRATCH_USAGE);
            assert_eq!((entry.desc.width, entry.desc.height), (96, 64));
        }
        let report = renderer.graph_report();
        let pooled: Vec<(u32, &str)> = report
            .graph
            .pooled
            .iter()
            .filter(|p| p.key != 0)
            .map(|p| (p.key, p.label.as_str()))
            .collect();
        assert_eq!(pooled.len(), 2, "{:?}", report.graph.pooled);
        let keyed = report.graph.keyed;
        assert_eq!(keyed.textures, 2, "{keyed:?}");
        let frame_pool = report.graph.transients;
        assert_eq!(frame_pool.textures + keyed.textures, report.graph.pooled.len());
        assert_eq!(
            frame_pool.bytes + keyed.bytes,
            report.graph.pooled.iter().map(|p| p.bytes).sum::<u64>()
        );
        let scratch_labels = HdrScratch::ALL.map(HdrScratch::label);
        for p in &report.graph.pooled {
            assert_eq!(
                p.key != 0,
                scratch_labels.contains(&p.label.as_str()),
                "only the scratch images are keyed: {p:?}"
            );
        }
        // The graph declares both as transients of their writers.
        for (node, resource) in [
            ("translucent_hdr", "translucent_hdr_scratch"),
            ("atmosphere", "outdoor_hdr_scratch"),
            ("clouds", "outdoor_hdr_scratch"),
            ("fog", "outdoor_hdr_scratch"),
        ] {
            let info = report.graph.nodes.iter().find(|n| n.name == node).expect("node");
            assert!(
                info.writes.iter().any(|w| w.starts_with(resource)),
                "{node} writes {:?}",
                info.writes
            );
        }
        assert!(report.graph.transient_writes.contains(&"translucent_hdr_scratch"));
        assert!(report.graph.transient_writes.contains(&"outdoor_hdr_scratch"));
        // Steady: the same two textures, nothing created.
        let created = renderer.keyed_transients.stats().created;
        renderer.render_offscreen(&device, &frame).expect("steady");
        assert_eq!(
            renderer.keyed_transients.stats().created,
            created,
            "a steady frame created a texture"
        );
        let again = scratch_entries(&renderer);
        assert_eq!(
            again.iter().map(|e| e.generation).collect::<Vec<_>>(),
            entries.iter().map(|e| e.generation).collect::<Vec<_>>()
        );
    }

    /// Every view post's bind groups name that is (or was) a scratch image is one the view's
    /// pool still holds: after a resize (the new size's scratch replaces the old) and after the
    /// old one's idle drop, post rebinds and keeps no group over a released texture.
    #[test]
    fn post_never_binds_a_released_or_replaced_hdr_scratch() {
        let device = crate::test_device();
        let quality = quality();
        let post = post();
        let mut renderer = renderer();
        // Every scratch view ever seen, by generation.
        let mut seen: Vec<(u64, wgpu::TextureView)> = Vec::new();
        let check = |renderer: &FrameRenderer, seen: &mut Vec<(u64, wgpu::TextureView)>, when: &str| {
            for entry in scratch_entries(renderer) {
                if !seen.iter().any(|(g, _)| *g == entry.generation)
                    && let Some(texture) = renderer
                        .keyed_transients
                        .latest(entry.key)
                        .filter(|t| t.generation == entry.generation)
                {
                    seen.push((entry.generation, texture.view.clone()));
                }
            }
            let held: Vec<u64> = renderer
                .keyed_transients
                .entries()
                .iter()
                .map(|(_, g)| *g)
                .collect();
            let bound = renderer
                .post
                .as_ref()
                .map(|post| post.bound_input_views())
                .unwrap_or_default();
            for (generation, view) in seen.iter() {
                if !held.contains(generation) {
                    assert!(
                        !bound.contains(view),
                        "{when}: post still binds the released scratch of generation {generation}"
                    );
                }
            }
        };
        let small = scene(96, 64);
        let mut frame = small.as_frame_scene_with_quality(quality);
        frame.post = Some(&post);
        for _ in 0..3 {
            renderer.render_offscreen(&device, &frame).expect("small");
            check(&renderer, &mut seen, "small");
        }
        let small_gens: Vec<u64> = scratch_entries(&renderer).iter().map(|e| e.generation).collect();
        assert_eq!(small_gens.len(), 2);
        // Post's cache names at least one scratch view while the screen is keyed (ADR-1361's
        // prewarm), so the release check below is not vacuous.
        let bound = renderer
            .post
            .as_ref()
            .map(|post| post.bound_input_views())
            .unwrap_or_default();
        assert!(
            seen.iter().any(|(_, view)| bound.contains(view)),
            "post prewarmed no scratch view"
        );
        // A resize: the new size gets its own pair; the old pair stays until its idle drop.
        let big = scene(128, 80);
        let mut frame = big.as_frame_scene_with_quality(quality);
        frame.post = Some(&post);
        renderer.render_offscreen(&device, &frame).expect("resized");
        check(&renderer, &mut seen, "the frame after the resize");
        let idle = crate::TransientPool::DEFAULT_IDLE_FRAMES;
        for frame_after in 0..=idle {
            renderer.render_offscreen(&device, &frame).expect("big");
            check(
                &renderer,
                &mut seen,
                &format!("frame {frame_after} after the resize"),
            );
        }
        let held: Vec<u64> = renderer
            .keyed_transients
            .entries()
            .iter()
            .map(|(_, g)| *g)
            .collect();
        for generation in &small_gens {
            assert!(
                !held.contains(generation),
                "the old-size scratch was never dropped"
            );
        }
        let big_entries = scratch_entries(&renderer);
        assert_eq!(big_entries.len(), 2, "{big_entries:?}");
        assert!(
            big_entries
                .iter()
                .all(|e| (e.desc.width, e.desc.height) == (128, 80))
        );
    }

    /// The main view's pooled bytes at 2560 × 1440, Desktop High, both scratch slots in use: the
    /// frame pool and the keyed pool (a measurement for the lane report; minutes on lavapipe).
    #[test]
    #[ignore = "1440p measurement on lavapipe (minutes)"]
    fn pooled_bytes_at_1440p() {
        let device = crate::test_device();
        let post = post();
        let lab = scene(2560, 1440);
        let mut frame = lab.as_frame_scene_with_quality(quality());
        frame.post = Some(&post);
        let mut renderer = renderer();
        for _ in 0..3 {
            renderer.render_offscreen(&device, &frame).expect("frame");
        }
        let report = renderer.graph_report();
        let (frame_pool, keyed) = (report.graph.transients, report.graph.keyed);
        for entry in &report.graph.pooled {
            println!(
                "pooled {:>2} {:<32} {}x{} {:?} {} bytes",
                entry.key, entry.label, entry.desc.width, entry.desc.height, entry.desc.format, entry.bytes
            );
        }
        println!(
            "1440p frame pool: {} textures, {} bytes; keyed pool: {} textures, {} bytes; total {} bytes",
            frame_pool.textures,
            frame_pool.bytes,
            keyed.textures,
            keyed.bytes,
            frame_pool.bytes + keyed.bytes
        );
        assert_eq!(keyed.textures, 2);
    }

    /// The latest scratch of each slot is kept through frames that do not draw it (post's
    /// groups are built ahead of the first held frame, ADR-1361), but another key's or an
    /// unkeyed acquire of the same description never gets it.
    #[test]
    fn keep_latest_holds_a_slot_through_frames_that_skip_it() {
        let device = crate::test_device();
        let d = device.tracked();
        let mut pool = crate::TransientPool::default();
        let desc =
            crate::TextureDesc::new(8, 8, wgpu::TextureFormat::Rgba16Float).with_usage(HDR_SCRATCH_USAGE);
        pool.begin_frame(1);
        let a = pool.acquire_keyed(&d, &desc, HdrScratch::Translucent.key(), "a");
        let b = pool.acquire_keyed(&d, &desc, HdrScratch::Outdoor.key(), "b");
        assert_ne!(a.generation, b.generation);
        for frame in 2..20 {
            pool.begin_frame(frame);
            for slot in HdrScratch::ALL {
                assert!(pool.keep_latest(slot.key()));
            }
            // An unkeyed acquire of the same description makes its own texture.
            let plain = pool.acquire(&d, &desc, "plain");
            assert_ne!(plain.generation, a.generation);
            assert_ne!(plain.generation, b.generation);
        }
        assert_eq!(
            pool.latest(HdrScratch::Translucent.key()).map(|t| t.generation),
            Some(a.generation)
        );
        assert_eq!(
            pool.latest(HdrScratch::Outdoor.key()).map(|t| t.generation),
            Some(b.generation)
        );
        assert_eq!(pool.stats().created, 3, "a, b and one unkeyed texture");
        // A new size for slot 0: the old one is no longer the latest and ages out.
        let bigger =
            crate::TextureDesc::new(16, 8, wgpu::TextureFormat::Rgba16Float).with_usage(HDR_SCRATCH_USAGE);
        pool.begin_frame(20);
        let a2 = pool.acquire_keyed(&d, &bigger, HdrScratch::Translucent.key(), "a");
        for frame in 21..(21 + crate::TransientPool::DEFAULT_IDLE_FRAMES + 2) {
            pool.begin_frame(frame);
            for slot in HdrScratch::ALL {
                pool.keep_latest(slot.key());
            }
        }
        let held: Vec<u64> = pool.entries().iter().map(|(_, g)| *g).collect();
        assert!(
            !held.contains(&a.generation),
            "the replaced slot-0 texture was kept"
        );
        assert!(held.contains(&a2.generation));
        assert!(held.contains(&b.generation));
        assert!(!pool.keep_latest(0), "key 0 holds nothing");
    }
}
