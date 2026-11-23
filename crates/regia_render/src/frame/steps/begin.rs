// SPDX-License-Identifier: MPL-2.0
//! The opening of `FrameRenderer::render_into` (ADR-2300): the frame clock, the per-frame pools,
//! the adapter's memory class, the non-blocking device poll with the timings readback, and (inside
//! the `encode` span) the mesh belt sweep, the draw window and the texture streaming budget. Moved
//! out of `frame.rs` unchanged.

use crate::frame::*;

/// The frame clock and the CPU time the opening spent in readbacks.
pub(in crate::frame) struct FrameOpen {
    /// The frame renders into a texture view, not the main view.
    pub(in crate::frame) view_pass: bool,
    /// The frame clock (`FrameRenderer::frame_index` after this frame's step).
    pub(in crate::frame) now: u64,
    /// Frames since the last camera cut: the jitter and noise phase.
    pub(in crate::frame) temporal_index: u64,
    /// When the frame started.
    pub(in crate::frame) frame_started: std::time::Instant,
    /// CPU time of the frame-begin poll and timings readback (ms).
    pub(in crate::frame) readback_ms: f32,
}

impl FrameRenderer {
    /// Starts the frame: resets the last submission and screen report, steps the frame clock
    /// (the main view only), starts the transient pool, breadcrumbs, material arena and scratch
    /// lists, measures the adapter once, and polls the device for finished timings.
    pub(in crate::frame) fn open_frame(
        &mut self,
        device: &RenderDevice,
        d: &crate::TrackedDevice,
        scene: &FrameScene<'_>,
        target: &FrameTarget<'_>,
    ) -> FrameOpen {
        self.last_submission = None;
        self.screen_report = ScreenPassReport::default();
        self.begin_cut_frame(scene, target);
        // A texture view is part of the host's frame, not a frame of its own: the frame clock
        // (temporal jitter and noise, cache ages, sweeps) counts the main view only.
        let view_pass = self.view_pass.is_some();
        if !view_pass {
            self.frame_index += 1;
        }
        let now = self.frame_index;
        // The frame-debugger draw log (ADR-8224): on only for the main-view frame an armed pass
        // capture copies; every other frame opens it off (a flag; no allocation).
        crate::frame_stats::open_draw_log(
            !view_pass
                && !self.capturing_probe
                && self
                    .extension_passes
                    .pass_copies
                    .as_deref()
                    .is_some_and(|copies| copies.records_draws_of(now)),
        );
        // The view's transient pool starts its frame (ADR-2018). A texture it drops (a size or
        // format left for `TransientPool::DEFAULT_IDLE_FRAMES` pool frames) renews the bind
        // epoch, as releasing an idle target pair does, so no cached group names it.
        if self.transients.begin_frame(now) {
            self.bind_epoch = crate::bind_cache::new_epoch();
        }
        // The keyed pool likewise; the HDR scratch images outlive the frames that do not draw
        // them (ADR-7877).
        if self.begin_hdr_scratch_frame(now) {
            self.bind_epoch = crate::bind_cache::new_epoch();
        }
        crate::breadcrumbs::begin_frame(now);
        // Timestamp-backed breadcrumbs (opt-in, ADR-2307) reuse the GPU timing query set.
        if self.gpu_timer.is_none()
            && crate::breadcrumbs::global().timestamp_mode()
                == crate::breadcrumbs::TimestampMode::WhenSupported
        {
            self.set_gpu_timing(device, true);
        }
        // Material arena slots freed `SLOT_RECYCLE_FRAMES` frames ago may take a new material.
        if let Some(arena) = self.material_arena.as_mut() {
            arena.reclaim(now);
        }
        self.scratch.begin_frame(self.culler.scratch_growths());
        if self.adapter_vram_mib.is_none() {
            let adapter = crate::quality::AdapterProfile::from_device(device);
            self.adapter_vram_mib = Some(adapter.vram_mb);
            self.adapter_discrete = adapter.class == crate::quality::DeviceClass::Discrete;
            self.adapter_integrated = crate::low_memory::integrated_limits_apply_sized(
                adapter.class,
                adapter.vendor,
                adapter.device,
                adapter.vram_mb,
            );
        }
        // Jitter and noise phase count from the last camera cut (a cut restarts them).
        let temporal_index = self.frame_index.wrapping_sub(self.cut.origin);
        let frame_started = std::time::Instant::now();
        let mut readback_ms = 0.0f32;
        {
            let _begin = tracing::info_span!("frame_begin").entered();
            let pump = std::time::Instant::now();
            if d.poll(wgpu::PollType::Poll).is_err() {
                tracing::warn!(target: "regia_render", "non-blocking device poll failed");
            }
            self.collect_timings(device, crate::readback::READBACK_LATENCY);
            readback_ms += pump.elapsed().as_secs_f32() * 1000.0;
        }
        FrameOpen {
            view_pass,
            now,
            temporal_index,
            frame_started,
            readback_ms,
        }
    }

    /// The first step inside the `encode` span: drops the meshes the last submit's belt copied,
    /// opens the draw window and the belt, and sets the frame's texture streaming budget.
    pub(in crate::frame) fn begin_encode(&mut self, d: &wgpu::Device, scene: &FrameScene<'_>) {
        for key in std::mem::take(&mut self.mesh_belt_keys) {
            self.meshes.remove(&key);
            self.meshes.remove(&(key ^ LOD_DITHER_MESH_TAG));
        }
        // From here to the sweep every entry a draw refers to stays (`lifecycle`, W2.2).
        self.open_draw_window();
        self.belt.begin_frame();
        // A renderer that reserved instances ([`Self::reserve_instances`]) makes the belt's room for
        // them on its first frame, which is its warm-up. Nothing is missing on the frames after it.
        if self.culler.reserved() > 0 {
            self.belt.warm_ring(d, belt_reserve_bytes(self.culler.reserved()));
        }
        self.mesh_belt_bytes = 0;
        // Out-of-memory errors lower the streaming budget (`gpu_health`); an integrated GPU gets
        // its own smaller one (`low_memory`, ADR-223).
        let mut streaming = scene.quality.texture_streaming;
        if !self.vram_budgets_off {
            streaming.budget_mib =
                streaming.vram_scaled_budget_mib(self.adapter_vram_mib.flatten(), self.adapter_discrete);
        }
        if self.adapter_integrated {
            streaming = streaming.for_integrated_gpu(
                self.adapter_vram_mib.flatten(),
                crate::low_memory::system_ram_mib(),
            );
        }
        streaming.budget_mib = crate::gpu_health::stepped_budget_mib(
            streaming.budget_mib,
            crate::gpu_health::texture_budget_steps(),
        );
        if self.complete_frames.is_some() {
            streaming = streaming.complete();
        }
        // A texture view is part of the host's frame: it does not step the back-off's frames.
        self.stream_frame.begin(streaming, self.view_pass.is_none());
        crate::texture_budget::set_resident_cap(scene.quality.texture_streaming);
    }
}
