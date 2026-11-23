// SPDX-License-Identifier: MPL-2.0
//! Per-camera state of render-to-texture views (ADR-184 "Texture views keep their own view
//! state").
//!
//! [`FrameRenderer`] keeps everything that depends on the camera or the target size in its own
//! fields: the size-keyed targets, temporal history, the shadow maps and their caches, the
//! screen-space and post passes, the cut detector, the last frame's reports, and the uniform
//! buffers it fills from its eye (frame, lights, cascades, shadow tiles, ground bounce, mesh
//! occluders, emissive probes) with the byte mirrors that skip unchanged uploads. A texture
//! view swaps its own [`ViewState`] into those fields for its frame and swaps the main view's
//! back after, so a cockpit screen or a map camera neither reallocates the window's targets,
//! invalidates its history or its paged shadows, nor makes it re-upload its uniforms. Meshes,
//! materials, textures, pipelines, the sun, cluster and GI buffers stay shared. The GPU culler's
//! pipelines and Hi-Z pyramid stay shared too, but each view swaps in its own cull buffers,
//! mirrors and groups ([`crate::gpu_driven::CullViewData`]): a view draws through the same
//! GPU-driven batches as the main view, frustum-culled with its own camera, never tested
//! against the main view's pyramid (ADR-184 addendum "Texture views draw through the
//! GPU-driven batches").

use std::collections::HashMap;

use super::{
    CasterListCache, DepthMatch, DrawStats, FrameCpuTimings, FrameExtent, FrameRenderer, GpuTimer,
    GpuTimings, HdrCopy, ScreenCacheKey, ScreenPassReport, ShadowCacheReport, TargetPair, translucent_shadow,
};

/// One texture view's camera- and size-dependent renderer state.
pub(crate) struct ViewState {
    csm: Option<(u32, u32, Vec<wgpu::TextureView>)>,
    atlas: Option<(u32, wgpu::TextureView)>,
    tile_vp_bufs: Vec<wgpu::Buffer>,
    tile_vp_bgs: Vec<wgpu::BindGroup>,
    tile_vp_bytes: Vec<Vec<u8>>,
    targets: HashMap<(u32, u32), TargetPair>,
    post: Option<crate::post_pass::PostChain>,
    lens: crate::lens::LensPasses,
    view_bgs: crate::bind_cache::BindCache<(u64, u64)>,
    cluster_indices_view_gen: u64,
    decal_receiver: crate::decal_receiver::DecalReceiver,
    gpu_timer: Option<GpuTimer>,
    pending_timer_slot: Option<usize>,
    last_timings: GpuTimings,
    last_cpu: FrameCpuTimings,
    last_draw_stats: DrawStats,
    last_shadow_casters: usize,
    vis_shadow_last: bool,
    vis_set_eye: Option<[f32; 3]>,
    ssao_pass: crate::ssao::SsaoPass,
    contact_pass: crate::contact_shadows::ContactShadowPass,
    taau_pass: crate::taau::TaauPass,
    prev_models: crate::key_hash::KeyMap<u64, [[f32; 4]; 4]>,
    prev_palettes: crate::key_hash::KeyMap<u64, (u32, u32)>,
    prev_joints: Vec<crate::JointMatrix>,
    prev_view_proj: [[f32; 4]; 4],
    have_prev_vp: bool,
    taau_ready: bool,
    taau_size: (u32, u32),
    tsr_pass: crate::tsr::TsrPass,
    fsr3_pass: crate::fsr3::Fsr3Pass,
    vendor_upscaler: crate::upscaler::vendor::dlss::FrameVendor,
    texture_lod_bias: f32,
    sun_pages: crate::paged_shadow::SunPageCache,
    atlas_tiles: crate::paged_shadow::AtlasTileCache,
    csm_valid: bool,
    atlas_valid: bool,
    had_unbounded_caster: bool,
    near_tile_vp: Option<[[f32; 4]; 4]>,
    shadow_report: ShadowCacheReport,
    caster_list_cache: CasterListCache,
    light_upload: crate::light_budget::LightUploadReport,
    light_warnings: crate::light_budget::LightDropWarnings,
    emissive_report: crate::emissive_lights::EmissiveLightReport,
    frame_problems: Vec<crate::host_checklist::FrameProblem>,
    last_extent: Option<FrameExtent>,
    highend: crate::highend::HighEndPass,
    screen_cache_key: Option<ScreenCacheKey>,
    screen_cache_view: Option<wgpu::TextureView>,
    screen_hold: u32,
    screen_report: ScreenPassReport,
    depth_match: Option<DepthMatch>,
    transients: crate::TransientPool,
    keyed_transients: crate::TransientPool,
    shared_shadow_keys: [Option<([[f32; 4]; 4], u64)>; 4],
    cascade_anchors: [Option<crate::cascade_anchor::CascadeAnchor>; 4],
    near_anchor: Option<[f32; 3]>,
    shared_shadow_draws: u64,
    shared_shadow_region_draws: u64,
    shared_upload_last: (u32, u64),
    last_submission: Option<wgpu::SubmissionIndex>,
    hdr_capture_requested: bool,
    hdr_capture: Option<(HdrCopy, f32)>,
    prev_pre_exposure: f32,
    cut: crate::camera_cut::CutState,
    translucent_maps: translucent_shadow::TranslucentMaps,
    translucent_view_gen: u64,
    cloud_map_view_gen: u64,
    ibl_view_gen: u64,
    frame_buf: Option<wgpu::Buffer>,
    frame_bytes: Vec<u8>,
    lights_buf: Option<wgpu::Buffer>,
    lights_bytes: Vec<u8>,
    csm_buf: Option<wgpu::Buffer>,
    csm_bytes: Vec<u8>,
    tiles_buf: Option<wgpu::Buffer>,
    tiles_bytes: Vec<u8>,
    ground_bounce_buf: Option<wgpu::Buffer>,
    ground_bounce_bytes: Vec<u8>,
    mesh_occluder_buf: Option<wgpu::Buffer>,
    mesh_occluder_bytes: Vec<u8>,
    probe_atlas: Option<(wgpu::Texture, wgpu::TextureView)>,
    probe_atlas_bytes: Vec<u8>,
    /// The view's GPU-cull buffers, mirrors and groups (ADR-184 addendum).
    cull: crate::gpu_driven::CullViewData,
}

impl ViewState {
    /// Starts the view's temporal history over.
    pub(crate) fn forget_history(&mut self) {
        self.have_prev_vp = false;
        self.prev_models.clear();
    }

    /// Returns the dropped view's size-dependent targets to the device now.
    pub(crate) fn release(self) {
        for (_, pair) in self.targets {
            pair.destroy();
        }
    }

    /// A view that has not rendered yet. It never detects or takes a camera cut: those are the
    /// host view's.
    pub(crate) fn new() -> Self {
        let mut cut = crate::camera_cut::CutState::default();
        cut.suspended = 1;
        Self {
            csm: None,
            atlas: None,
            tile_vp_bufs: Vec::new(),
            tile_vp_bgs: Vec::new(),
            tile_vp_bytes: Vec::new(),
            targets: HashMap::new(),
            post: None,
            lens: Default::default(),
            view_bgs: Default::default(),
            cluster_indices_view_gen: 0,
            decal_receiver: Default::default(),
            gpu_timer: None,
            pending_timer_slot: None,
            last_timings: GpuTimings::default(),
            last_cpu: FrameCpuTimings::default(),
            last_draw_stats: DrawStats::default(),
            last_shadow_casters: 0,
            vis_shadow_last: false,
            vis_set_eye: None,
            ssao_pass: Default::default(),
            contact_pass: Default::default(),
            taau_pass: Default::default(),
            prev_models: Default::default(),
            prev_palettes: Default::default(),
            prev_joints: Vec::new(),
            prev_view_proj: [[0.0; 4]; 4],
            have_prev_vp: false,
            taau_ready: false,
            taau_size: (0, 0),
            tsr_pass: Default::default(),
            fsr3_pass: Default::default(),
            vendor_upscaler: Default::default(),
            texture_lod_bias: 0.0,
            sun_pages: Default::default(),
            atlas_tiles: Default::default(),
            csm_valid: false,
            atlas_valid: false,
            had_unbounded_caster: false,
            near_tile_vp: None,
            shadow_report: ShadowCacheReport::default(),
            caster_list_cache: CasterListCache::default(),
            light_upload: Default::default(),
            light_warnings: Default::default(),
            emissive_report: Default::default(),
            frame_problems: Vec::new(),
            last_extent: None,
            highend: Default::default(),
            screen_cache_key: None,
            screen_cache_view: None,
            screen_hold: 0,
            screen_report: ScreenPassReport::default(),
            depth_match: None,
            transients: crate::TransientPool::default(),
            keyed_transients: crate::TransientPool::default(),
            shared_shadow_keys: [None; 4],
            cascade_anchors: [None; 4],
            near_anchor: None,
            shared_shadow_draws: 0,
            shared_shadow_region_draws: 0,
            shared_upload_last: (0, 0),
            last_submission: None,
            hdr_capture_requested: false,
            hdr_capture: None,
            prev_pre_exposure: 0.0,
            cut,
            translucent_maps: Default::default(),
            translucent_view_gen: 0,
            cloud_map_view_gen: 0,
            ibl_view_gen: 0,
            frame_buf: None,
            frame_bytes: Vec::new(),
            lights_buf: None,
            lights_bytes: Vec::new(),
            csm_buf: None,
            csm_bytes: Vec::new(),
            tiles_buf: None,
            tiles_bytes: Vec::new(),
            ground_bounce_buf: None,
            ground_bounce_bytes: Vec::new(),
            mesh_occluder_buf: None,
            mesh_occluder_bytes: Vec::new(),
            probe_atlas: None,
            probe_atlas_bytes: Vec::new(),
            cull: crate::gpu_driven::CullViewData::texture_view(),
        }
    }

    /// Shared cluster storage can grow while this view is dormant.
    pub(super) fn invalidate_cluster_bindings(&mut self, generation: u64) {
        self.view_bgs.clear();
        self.cluster_indices_view_gen = generation;
    }

    /// GPU timings of this view's last timed frame (zeros before the first, or without
    /// timing). While the view is swapped out these are its own.
    pub(crate) fn gpu_timings(&self) -> GpuTimings {
        self.last_timings
    }

    /// CPU phases of this view's last frame.
    pub(crate) fn cpu_timings(&self) -> FrameCpuTimings {
        self.last_cpu
    }

    /// Paged sun-shadow redraw report of this view's last frame.
    pub(crate) fn shadow_report(&self) -> &ShadowCacheReport {
        &self.shadow_report
    }
}

impl FrameRenderer {
    /// Exchanges the active view's camera- and size-dependent state with `view`. Swapping
    /// twice restores both.
    pub(crate) fn swap_view_state(&mut self, view: &mut ViewState) {
        use std::mem::swap;
        swap(&mut self.csm, &mut view.csm);
        swap(&mut self.atlas, &mut view.atlas);
        swap(&mut self.tile_vp_bufs, &mut view.tile_vp_bufs);
        swap(&mut self.tile_vp_bgs, &mut view.tile_vp_bgs);
        swap(&mut self.tile_vp_bytes, &mut view.tile_vp_bytes);
        swap(&mut self.targets, &mut view.targets);
        swap(&mut self.post, &mut view.post);
        swap(&mut self.lens, &mut view.lens);
        swap(&mut self.view_bgs, &mut view.view_bgs);
        swap(
            &mut self.cluster_indices_view_gen,
            &mut view.cluster_indices_view_gen,
        );
        // A texture-view render may grow the shared buffer while the main view is held in
        // the caller's local ViewState, outside texture_views. Validate both sides of a swap.
        if self.cluster_indices_view_gen != self.cluster_indices_gen {
            self.view_bgs.clear();
            self.cluster_indices_view_gen = self.cluster_indices_gen;
        }
        if view.cluster_indices_view_gen != self.cluster_indices_gen {
            view.invalidate_cluster_bindings(self.cluster_indices_gen);
        }
        swap(&mut self.decal_receiver, &mut view.decal_receiver);
        swap(&mut self.gpu_timer, &mut view.gpu_timer);
        swap(&mut self.pending_timer_slot, &mut view.pending_timer_slot);
        swap(&mut self.last_timings, &mut view.last_timings);
        swap(&mut self.last_cpu, &mut view.last_cpu);
        swap(&mut self.last_draw_stats, &mut view.last_draw_stats);
        swap(&mut self.last_shadow_casters, &mut view.last_shadow_casters);
        swap(&mut self.vis_shadow_last, &mut view.vis_shadow_last);
        swap(&mut self.vis_set_eye, &mut view.vis_set_eye);
        swap(&mut self.ssao_pass, &mut view.ssao_pass);
        swap(&mut self.contact_pass, &mut view.contact_pass);
        swap(&mut self.taau_pass, &mut view.taau_pass);
        swap(&mut self.prev_models, &mut view.prev_models);
        swap(&mut self.prev_palettes, &mut view.prev_palettes);
        swap(&mut self.prev_joints, &mut view.prev_joints);
        swap(&mut self.prev_view_proj, &mut view.prev_view_proj);
        swap(&mut self.have_prev_vp, &mut view.have_prev_vp);
        swap(&mut self.taau_ready, &mut view.taau_ready);
        swap(&mut self.taau_size, &mut view.taau_size);
        swap(&mut self.tsr_pass, &mut view.tsr_pass);
        swap(&mut self.fsr3_pass, &mut view.fsr3_pass);
        swap(&mut self.vendor_upscaler, &mut view.vendor_upscaler);
        swap(&mut self.texture_lod_bias, &mut view.texture_lod_bias);
        swap(&mut self.sun_pages, &mut view.sun_pages);
        swap(&mut self.atlas_tiles, &mut view.atlas_tiles);
        swap(&mut self.csm_valid, &mut view.csm_valid);
        swap(&mut self.atlas_valid, &mut view.atlas_valid);
        swap(&mut self.had_unbounded_caster, &mut view.had_unbounded_caster);
        swap(&mut self.near_tile_vp, &mut view.near_tile_vp);
        swap(&mut self.shadow_report, &mut view.shadow_report);
        swap(&mut self.caster_list_cache, &mut view.caster_list_cache);
        swap(&mut self.light_upload, &mut view.light_upload);
        swap(&mut self.light_warnings, &mut view.light_warnings);
        swap(&mut self.emissive_report, &mut view.emissive_report);
        swap(&mut self.frame_problems, &mut view.frame_problems);
        swap(&mut self.last_extent, &mut view.last_extent);
        swap(&mut self.highend, &mut view.highend);
        swap(&mut self.screen_cache_key, &mut view.screen_cache_key);
        swap(&mut self.screen_cache_view, &mut view.screen_cache_view);
        swap(&mut self.screen_hold, &mut view.screen_hold);
        swap(&mut self.screen_report, &mut view.screen_report);
        swap(&mut self.depth_match, &mut view.depth_match);
        swap(&mut self.transients, &mut view.transients);
        swap(&mut self.keyed_transients, &mut view.keyed_transients);
        swap(&mut self.shared_shadow_keys, &mut view.shared_shadow_keys);
        swap(&mut self.cascade_anchors, &mut view.cascade_anchors);
        swap(&mut self.near_anchor, &mut view.near_anchor);
        swap(&mut self.shared_shadow_draws, &mut view.shared_shadow_draws);
        swap(
            &mut self.shared_shadow_region_draws,
            &mut view.shared_shadow_region_draws,
        );
        swap(&mut self.shared_upload_last, &mut view.shared_upload_last);
        swap(&mut self.last_submission, &mut view.last_submission);
        swap(&mut self.hdr_capture_requested, &mut view.hdr_capture_requested);
        swap(&mut self.hdr_capture, &mut view.hdr_capture);
        swap(&mut self.prev_pre_exposure, &mut view.prev_pre_exposure);
        swap(&mut self.cut, &mut view.cut);
        swap(&mut self.translucent_maps, &mut view.translucent_maps);
        swap(&mut self.translucent_view_gen, &mut view.translucent_view_gen);
        swap(&mut self.cloud_map_view_gen, &mut view.cloud_map_view_gen);
        swap(&mut self.ibl_view_gen, &mut view.ibl_view_gen);
        swap(&mut self.frame_buf, &mut view.frame_buf);
        swap(&mut self.frame_bytes, &mut view.frame_bytes);
        swap(&mut self.lights_buf, &mut view.lights_buf);
        swap(&mut self.lights_bytes, &mut view.lights_bytes);
        swap(&mut self.csm_buf, &mut view.csm_buf);
        swap(&mut self.csm_bytes, &mut view.csm_bytes);
        swap(&mut self.tiles_buf, &mut view.tiles_buf);
        swap(&mut self.tiles_bytes, &mut view.tiles_bytes);
        swap(&mut self.ground_bounce_buf, &mut view.ground_bounce_buf);
        swap(&mut self.ground_bounce_bytes, &mut view.ground_bounce_bytes);
        swap(&mut self.mesh_occluder_buf, &mut view.mesh_occluder_buf);
        swap(&mut self.mesh_occluder_bytes, &mut view.mesh_occluder_bytes);
        swap(&mut self.probe_atlas, &mut view.probe_atlas);
        swap(&mut self.probe_atlas_bytes, &mut view.probe_atlas_bytes);
        self.culler.swap_view_data(&mut view.cull);
    }
}
