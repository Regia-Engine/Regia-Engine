// SPDX-License-Identifier: MPL-2.0
//! GPU residency feedback on the frame renderer (bindless step 5, ADR-1110,
//! `crate::residency_feedback`).
//!
//! Once per main-view frame, before the stream pass, the texture table hands over the newest
//! mapped readout of the feedback words. Each streamed material on the table path takes the
//! largest need recorded at its streamed slots' indices: in `observe` mode it is only reported,
//! in `drive` mode the stream also takes it as one more draw's need.

use super::FrameRenderer;
use crate::residency_feedback::{FeedbackMode, MaterialResidency, ResidencyFeedbackStats};
use crate::texture_stream::STREAMED_SLOTS;

impl FrameRenderer {
    /// Reads and applies this frame's residency feedback (nothing while feedback is off).
    pub(super) fn residency_feedback_frame(
        &mut self,
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        now: u64,
    ) {
        // End the table borrow before the belt and the material map (different fields).
        // The copy is queued even when nothing is mapped yet (`feedback_frame` is `None`).
        let (applied, op, drive) = {
            let Some(table) = self.texture_table.as_mut() else {
                return;
            };
            let applied = table.feedback_frame(d, q);
            let op = table.feedback_mut().and_then(|feedback| feedback.take_gpu_op());
            let drive = table.feedback().is_some_and(|f| f.mode() == FeedbackMode::Drive);
            (applied, op, drive)
        };
        if let Some(op) = op {
            self.belt.defer_feedback(op);
        }
        let Some((readout, stale)) = applied else {
            return;
        };
        let _span = tracing::info_span!("residency_feedback_apply").entered();
        let mut materials = Vec::new();
        let mut driven = 0;
        for (&material_key, entry) in self.materials.iter_mut() {
            let (Some(stream), Some(indices)) = (entry.stream.as_mut(), entry.table) else {
                continue;
            };
            let Some(gpu_need_px) = indices.0[..STREAMED_SLOTS]
                .iter()
                .filter_map(|&index| readout.need_px(index))
                .reduce(f32::max)
            else {
                continue;
            };
            materials.push(MaterialResidency {
                material_key,
                gpu_need_px,
                cpu_need_px: stream.need_px_at(now),
                resident_px: stream.resident_px(),
            });
            if drive {
                stream.note_need(gpu_need_px, now);
                driven += 1;
            }
        }
        let textures = readout.reported();
        if let Some(feedback) = self.texture_table.as_mut().and_then(|table| table.feedback_mut()) {
            feedback.note_applied(stale, textures, driven, materials);
            feedback.recycle(readout);
        }
    }

    /// Maps the residency-feedback copy after the submit that recorded it. No-op while
    /// feedback is off or this frame queued nothing.
    pub(in crate::frame) fn map_feedback_after_submit(&mut self) {
        if let Some(feedback) = self.texture_table.as_mut().and_then(|table| table.feedback_mut()) {
            feedback.map_after_submit();
        }
    }

    /// What GPU residency feedback did (ADR-1110): copies, readouts, stale words, and the last
    /// readout's streamed materials with their GPU and CPU needs, largest mismatch first.
    /// `None` while feedback is off (`REGIA_RESIDENCY_FEEDBACK`), off the texture table path,
    /// or before the first standard material.
    #[must_use]
    pub fn residency_feedback_stats(&self) -> Option<ResidencyFeedbackStats> {
        self.texture_table
            .as_ref()
            .and_then(|table| table.feedback())
            .map(|feedback| feedback.stats().clone())
    }
}

/// GPU checks of bindless step 5 (lavapipe; each skips without an adapter that has the table
/// features, and the partial-prefix ones without `PARTIALLY_BOUND_BINDING_ARRAY`). None needs a
/// golden: every pixel check compares against the per-material path on the same adapter.
#[cfg(test)]
mod gpu_tests {
    use crate::residency_feedback::{FeedbackMode, test_override};
    use crate::texture_table::{
        RESERVED_INDICES, TEXTURE_TABLE_LEVEL, TextureTable, device_features_for_level,
    };
    use crate::{
        DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial, LabMesh,
        LabScene, LabTexture, LabVertex, RenderDevice,
    };

    /// The devices of these tests, all on the shared headless adapter with its optional features,
    /// differing only in the table features, partial binding and the feedback mode.
    struct Devices {
        /// Table features and partially bound arrays (`None` when the adapter lacks the latter).
        partial: Option<RenderDevice>,
        /// Table features, feedback in `observe`.
        observe: RenderDevice,
        /// Table features, feedback in `drive`.
        drive: RenderDevice,
        /// Neither: the per-material path.
        classic: RenderDevice,
    }

    fn open(
        base: &RenderDevice,
        features: wgpu::Features,
        mode: Option<FeedbackMode>,
    ) -> Option<RenderDevice> {
        let (device, queue) =
            regia_core::tasks::block_on(base.adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("bindless step 5 test"),
                required_limits: base.device.limits(),
                required_features: features,
                memory_hints: crate::device::device_memory_hints(&base.adapter.get_info()),
                ..Default::default()
            }))
            .ok()?;
        if let Some(mode) = mode {
            test_override::set(&device, mode);
        }
        let capabilities = crate::RenderCapabilities::from_adapter(&base.adapter, &device);
        Some(RenderDevice::from_parts(
            base.instance.clone(),
            base.adapter.clone(),
            device,
            queue,
            capabilities,
        ))
    }

    fn open_devices() -> Option<Devices> {
        let base = RenderDevice::shared_headless().ok()?;
        let adapter = base.adapter.features();
        let table = device_features_for_level(TEXTURE_TABLE_LEVEL, adapter);
        if table.is_empty() {
            return None;
        }
        let partial_feature = crate::table_heap::PARTIAL_FEATURE;
        let plain = base.device.features() - partial_feature;
        let observe = open(&base, plain | table, Some(FeedbackMode::Observe))?;
        if crate::texture_table::device_support(&observe.device)
            .capacity()
            .is_none()
        {
            eprintln!("skipping: the device stays off the table");
            return None;
        }
        let drive = open(&base, plain | table, Some(FeedbackMode::Drive))?;
        let classic = open(&base, plain - table, None)?;
        let partial = adapter
            .contains(partial_feature)
            .then(|| open(&base, plain | table | partial_feature, None))
            .flatten();
        Some(Devices {
            partial,
            observe,
            drive,
            classic,
        })
    }

    /// One set per test process (test threads that each open devices can livelock the NVIDIA
    /// driver; `RenderDevice::shared_headless` does the same).
    fn devices() -> Option<&'static Devices> {
        static DEVICES: std::sync::OnceLock<Option<Devices>> = std::sync::OnceLock::new();
        DEVICES.get_or_init(open_devices).as_ref()
    }

    fn solid(device: &wgpu::Device, label: &str) -> wgpu::TextureView {
        device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&wgpu::TextureViewDescriptor::default())
    }

    fn checker(a: [u8; 4], b: [u8; 4]) -> LabTexture {
        let mut rgba = Vec::with_capacity(16);
        for texel in [a, b, b, a] {
            rgba.extend_from_slice(&texel);
        }
        LabTexture {
            width: 2,
            height: 2,
            rgba,
        }
    }

    fn quad(x: f32, material: usize) -> LabMesh {
        LabMesh {
            vertices: vec![
                LabVertex::from_pos_normal_uv([x - 0.3, -0.3, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0]),
                LabVertex::from_pos_normal_uv([x + 0.3, -0.3, 0.0], [0.0, 0.0, 1.0], [1.0, 1.0]),
                LabVertex::from_pos_normal_uv([x + 0.3, 0.3, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0]),
                LabVertex::from_pos_normal_uv([x - 0.3, 0.3, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
            ],
            indices: vec![0, 1, 2, 0, 2, 3],
            material,
            ..LabMesh::unskinned()
        }
    }

    /// Three textured materials and an untextured one, sun shadows on.
    fn textured_scene() -> LabScene {
        let white = [255, 255, 255, 255];
        let dark = [40, 40, 40, 255];
        let materials = vec![
            LabMaterial {
                base_color: [0.8, 0.5, 0.25, 1.0],
                ..LabMaterial::default()
            },
            LabMaterial {
                albedo: Some(checker(white, dark)),
                emissive_map: Some(checker(dark, white)),
                emissive: [0.2, 0.1, 0.0],
                ..LabMaterial::default()
            },
            LabMaterial {
                albedo: Some(checker([200, 120, 60, 255], white)),
                normal_map: Some(checker([150, 128, 230, 255], [110, 128, 230, 255])),
                occlusion_map: Some(checker([255, 200, 90, 255], [180, 120, 40, 255])),
                roughness: 0.6,
                ..LabMaterial::default()
            },
        ];
        LabScene {
            width: 64,
            height: 32,
            eye: [0.0, 0.0, 3.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 0.9,
            near: 0.1,
            far: 20.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: false,
            exposure_ev: 12.0,
            ambient_lux: 5_000.0,
            double_sided: false,
            post: None,
            directional: DirectionalLightUniform::new([0.0, -0.3, -1.0], [1.0; 3], 20_000.0),
            local_lights: Vec::new(),
            materials,
            meshes: vec![quad(-1.2, 0), quad(-0.4, 1), quad(0.4, 2), quad(1.2, 1)],
        }
    }

    /// A `size`² pattern whose mip levels all differ.
    fn pattern(size: u32, seed: u32) -> LabTexture {
        let mut rgba = Vec::with_capacity((size * size * 4) as usize);
        for y in 0..size {
            for x in 0..size {
                let h = (x.wrapping_mul(73_856_093) ^ y.wrapping_mul(19_349_663) ^ seed)
                    .wrapping_mul(2_654_435_761);
                let checker = if ((x / 8) + (y / 8)).is_multiple_of(2) {
                    200
                } else {
                    60
                };
                rgba.extend_from_slice(&[checker, (h >> 24) as u8, ((x * 255) / size) as u8, 255]);
            }
        }
        LabTexture {
            width: size,
            height: size,
            rgba,
        }
    }

    /// Renders the streamed ground plane of the ADR-863 streaming test for `frames` frames on
    /// each of `devices` (the same fixed sequence on each) and returns the renderers and every
    /// frame's pixels per device.
    fn stream_frames(devices: &[&RenderDevice], frames: usize) -> Vec<(FrameRenderer, Vec<Vec<u8>>)> {
        let material = LabMaterial {
            albedo: Some(pattern(256, 1)),
            occlusion_map: Some(pattern(128, 2)),
            roughness: 0.6,
            ..LabMaterial::default()
        };
        let vertices: Vec<LabVertex> = [[-2.0f32, -2.0], [2.0, -2.0], [2.0, 2.0], [-2.0, 2.0]]
            .into_iter()
            .map(|[x, z]| LabVertex {
                position: [x, 0.0, z],
                normal: [0.0, 1.0, 0.0],
                uv: [x + 2.0, z + 2.0],
                tangent: [1.0, 0.0, 0.0, 1.0],
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            })
            .collect();
        let indices = [0u32, 2, 1, 0, 3, 2];
        let streaming = crate::TextureStreaming {
            enabled: true,
            initial_max_px: 16,
            max_px: 0,
            upload_kib_per_frame: 24,
            upload_us_per_frame: 1_000_000_000,
            fade_frames: 3,
            need_margin_percent: 200,
            resident_all: true,
            budget_mib: 0,
            eager: false,
        };
        let scene = FrameScene {
            width: 64,
            height: 64,
            eye: [0.3, 1.5, 0.4],
            target: [0.0, 0.0, -0.4],
            fov_y: 1.0,
            near: 0.05,
            far: 50.0,
            depth_prepass: false,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 0.0,
            ambient_lux: 0.0,
            double_sided: true,
            post: None,
            fog: crate::FogSettings::default(),
            directional: DirectionalLightUniform::new([0.3, -1.0, 0.2], [1.0, 1.0, 1.0], 1.0),
            local_lights: &[],
            quality: FrameQuality {
                texture_streaming: streaming,
                ..FrameQuality::LAB
            },
            meshes: vec![FrameMesh {
                is_static: false,
                mesh_key: 1,
                material_key: 1,
                object_key: 1,
                vertices: &vertices,
                indices: &indices,
                material: &material,
                model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: crate::ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            }],
        };
        devices
            .iter()
            .map(|device| {
                let mut renderer = FrameRenderer::new();
                let pixels = (0..frames)
                    .map(|frame| {
                        renderer
                            .render_offscreen(device, &scene)
                            .unwrap_or_else(|err| panic!("frame {frame}: {err:?}"))
                            .pixels
                    })
                    .collect();
                (renderer, pixels)
            })
            .collect()
    }

    /// The growth policy on a partially bound device: the first rebuild binds the minimum
    /// prefix, 1 100 textures grow it to 2 048, and after they leave it halves to 512 on the
    /// next rebuild that a change forced. The bind groups validate at every length.
    #[test]
    fn a_partially_bound_table_binds_the_prefix_the_scene_uses() {
        let Some(render) = devices().and_then(|d| d.partial.as_ref()) else {
            eprintln!("skipping: no adapter with the table features and partially bound arrays");
            return;
        };
        let device = &render.device;
        if crate::texture_table::device_support(device)
            .capacity()
            .is_none_or(|capacity| capacity < 2048 + RESERVED_INDICES)
        {
            eprintln!("skipping: the table is too small for this sequence");
            return;
        }
        let reserved = [
            solid(device, "white"),
            solid(device, "flat"),
            solid(device, "mid"),
            solid(device, "aux"),
        ];
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor::default());
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let mut table = TextureTable::new(
            device,
            [&reserved[0], &reserved[1], &reserved[2], &reserved[3]],
            &sampler,
            1,
        )
        .expect("table path");
        assert!(table.stats().partially_bound);
        table.refresh(device);
        assert_eq!(table.stats().bound, crate::table_heap::MIN_BOUND);
        let textures: Vec<wgpu::TextureView> = (0..1100).map(|i| solid(device, &format!("t{i}"))).collect();
        let sets: Vec<_> = textures
            .iter()
            .map(|view| {
                let views = [
                    view.clone(),
                    reserved[1].clone(),
                    reserved[0].clone(),
                    reserved[0].clone(),
                    reserved[2].clone(),
                    reserved[1].clone(),
                    reserved[3].clone(),
                ];
                table.acquire_set(&views)
            })
            .collect();
        assert_eq!(table.stats().high_water, RESERVED_INDICES + 1100);
        table.refresh(device);
        assert_eq!(table.stats().bound, 2048);
        for set in sets {
            table.release_set(set);
        }
        assert_eq!(table.stats().high_water, RESERVED_INDICES, "everything left");
        let views = [
            textures[0].clone(),
            reserved[1].clone(),
            reserved[0].clone(),
            reserved[0].clone(),
            reserved[2].clone(),
            reserved[1].clone(),
            reserved[3].clone(),
        ];
        let again = table.acquire_set(&views);
        assert_eq!(again.0[0], RESERVED_INDICES, "lowest index first");
        table.refresh(device);
        let stats = table.stats();
        assert_eq!(stats.bound, 512, "a quarter used: halved, with headroom");
        assert_eq!(stats.bound_changes, 2);
        assert_eq!(stats.rebuilds, 3);
        assert_eq!(
            stats.views_written,
            u64::from(crate::table_heap::MIN_BOUND) + 2048 + 512
        );
        let error = regia_core::tasks::block_on(scope.pop());
        assert!(error.is_none(), "validation error: {error:?}");
    }

    /// `swap_set`'s cases on a real table: a texture only this slot holds is replaced in place
    /// (same index, record unchanged); a shared texture, a view already in the table and a
    /// neutral view each move the slot; an unchanged slot keeps its index and its hold.
    #[test]
    fn residency_swaps_keep_indices_unless_shared() {
        let Some(devices) = devices() else {
            eprintln!("skipping: no adapter with the texture table features");
            return;
        };
        let device = &devices.observe.device;
        let reserved = [
            solid(device, "white"),
            solid(device, "flat"),
            solid(device, "mid"),
            solid(device, "aux"),
        ];
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor::default());
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let mut table = TextureTable::new(
            device,
            [&reserved[0], &reserved[1], &reserved[2], &reserved[3]],
            &sampler,
            1,
        )
        .expect("table path");
        let set = |albedo: &wgpu::TextureView, orm: &wgpu::TextureView| {
            [
                albedo.clone(),
                reserved[1].clone(),
                orm.clone(),
                reserved[0].clone(),
                reserved[2].clone(),
                reserved[1].clone(),
                reserved[3].clone(),
            ]
        };
        let (a1, a2, shared, s2, other) = (
            solid(device, "a1"),
            solid(device, "a2"),
            solid(device, "shared"),
            solid(device, "s2"),
            solid(device, "other"),
        );
        // Material A: own albedo, shared ORM. Material B: the shared ORM and another texture.
        let a = table.acquire_set(&set(&a1, &shared));
        let b = table.acquire_set(&set(&other, &shared));
        assert_eq!(a.0[0], 4);
        assert_eq!(a.0[2], 5);
        assert_eq!(b.0[2], 5, "shared by identity");
        assert_eq!(b.0[0], 6);
        table.refresh(device);
        let live = table.stats().live;

        // A's own albedo streams to a2: in place.
        let a_next = table.swap_set(a, &set(&a2, &shared));
        assert_eq!(a_next, a, "the record keeps its indices");
        assert_eq!(table.stats().in_place_swaps, 1);
        assert_eq!(table.stats().moved_swaps, 0);
        assert_eq!(table.stats().live, live);

        // A's shared ORM swaps to s2: B still holds `shared`, so A's slot moves.
        let a_moved = table.swap_set(a_next, &set(&a2, &s2));
        assert_ne!(a_moved.0[2], a_next.0[2], "a shared index never changes under B");
        assert_eq!(a_moved.0[0], a_next.0[0], "the unchanged slot keeps its index");
        assert_eq!(table.stats().moved_swaps, 1);

        // A's albedo swaps to a view B already holds: shares B's index.
        let a_shares = table.swap_set(a_moved, &set(&other, &s2));
        assert_eq!(a_shares.0[0], b.0[0]);
        assert_eq!(table.stats().moved_swaps, 2);

        // A's ORM drops to the neutral view: the reserved index.
        let a_neutral = table.swap_set(a_shares, &set(&other, &reserved[0]));
        assert_eq!(a_neutral.0[2], crate::texture_table::SLOT_FALLBACK[2]);
        assert_eq!(table.stats().moved_swaps, 3);

        // Releasing both leaves nothing behind.
        table.release_set(a_neutral);
        table.release_set(b);
        assert_eq!(table.stats().live, 0);
        assert_eq!(table.stats().high_water, RESERVED_INDICES);
        table.refresh(device);
        assert!(table.stats().rebuilds >= 2);
        let error = regia_core::tasks::block_on(scope.pop());
        assert!(error.is_none(), "validation error: {error:?}");
    }

    /// The prefix binding draws the same bytes as the per-material path, and binds less than
    /// the declared table.
    #[test]
    fn partial_prefix_draws_identical_pixels() {
        let Some(devices) = devices() else {
            eprintln!("skipping: no adapter with the texture table features");
            return;
        };
        let Some(partial) = devices.partial.as_ref() else {
            eprintln!("skipping: no partially bound arrays on this adapter");
            return;
        };
        let mut lab = textured_scene();
        let mut on_partial = FrameRenderer::new();
        let mut on_classic = FrameRenderer::new();
        for frame in 0..3 {
            lab.materials[0].roughness = 0.3 + 0.2 * frame as f32;
            let scene = lab.as_frame_scene_with_quality(FrameQuality::LAB);
            let a = on_partial
                .render_offscreen(partial, &scene)
                .expect("partial frame")
                .pixels;
            let b = on_classic
                .render_offscreen(&devices.classic, &scene)
                .expect("per-material frame")
                .pixels;
            let differing = a.iter().zip(&b).filter(|(x, y)| x != y).count();
            assert_eq!(differing, 0, "frame {frame}: {differing} bytes differ");
        }
        let stats = on_partial.texture_table_stats().expect("table stats");
        assert!(stats.partially_bound);
        assert!(stats.bound >= stats.high_water, "{stats:?}");
        assert_eq!(
            stats.bound,
            crate::table_heap::grow_target(stats.high_water, stats.capacity),
            "{stats:?}"
        );
        if stats.capacity > crate::table_heap::MIN_BOUND {
            assert!(stats.bound < stats.capacity, "{stats:?}");
        }
    }

    /// Residency swaps keep their table indices: over the ADR-863 streaming sequence every swap
    /// is in place, and with feedback observing, every frame is still byte-identical to the
    /// per-material path (observing changes neither pixels nor streaming). The feedback was
    /// read back and names the streamed material.
    #[test]
    fn observed_streaming_keeps_indices_and_pixels() {
        let Some(devices) = devices() else {
            eprintln!("skipping: no adapter with the texture table features");
            return;
        };
        if crate::residency_feedback::device_feedback_mode(&devices.observe.device) == FeedbackMode::Off {
            eprintln!("skipping: too few storage buffers per stage for feedback");
            return;
        }
        const FRAMES: usize = 40;
        let mut runs = stream_frames(&[&devices.observe, &devices.classic], FRAMES);
        let (classic, classic_pixels) = runs.pop().expect("classic run");
        let (observed, observed_pixels) = runs.pop().expect("observed run");
        for (frame, (a, b)) in observed_pixels.iter().zip(&classic_pixels).enumerate() {
            let differing = a.iter().zip(b).filter(|(x, y)| x != y).count();
            assert_eq!(
                differing, 0,
                "frame {frame}: {differing} bytes differ while streaming"
            );
        }
        assert!(
            observed_pixels.first() != observed_pixels.last(),
            "the stream swapped residency during the sequence"
        );
        let stats = observed.texture_table_stats().expect("table stats");
        assert!(stats.in_place_swaps > 0, "swaps happened: {stats:?}");
        assert_eq!(stats.moved_swaps, 0, "no streamed texture had to move: {stats:?}");
        assert!(stats.rebuilds <= stats.refreshes, "{stats:?}");
        assert!(classic.texture_table_stats().is_none());
        assert!(
            classic.residency_feedback_stats().is_none(),
            "no feedback off the table path"
        );
        let feedback = observed.residency_feedback_stats().expect("feedback on");
        assert_eq!(feedback.mode, FeedbackMode::Observe);
        // The first frame may build the table after its feedback point.
        assert!(
            (FRAMES as u64 - 1..=FRAMES as u64).contains(&(feedback.copies + feedback.skipped_copies)),
            "one copy (or a counted skip) per main-view frame: {feedback:?}"
        );
        assert!(feedback.readouts > 0, "the copies were read back: {feedback:?}");
        assert!(
            feedback.last_latency >= 1,
            "a readout is from an earlier frame: {feedback:?}"
        );
        assert_eq!(feedback.materials_driven, 0, "observe never drives");
        assert_eq!(feedback.materials_reported, 1, "the streamed plane: {feedback:?}");
        let plane = feedback.materials.first().expect("one material in the report");
        assert_eq!(plane.material_key, 1);
        assert!(
            (1.0..=65_536.0).contains(&plane.gpu_need_px),
            "a plausible GPU need: {plane:?}"
        );
        assert!(feedback.textures_reported >= 1, "{feedback:?}");
    }

    /// In `drive` mode the GPU's need reaches the stream: the plane is driven, and the run is
    /// free of validation errors.
    #[test]
    fn driven_feedback_reaches_the_stream() {
        let Some(devices) = devices() else {
            eprintln!("skipping: no adapter with the texture table features");
            return;
        };
        if crate::residency_feedback::device_feedback_mode(&devices.drive.device) == FeedbackMode::Off {
            eprintln!("skipping: too few storage buffers per stage for feedback");
            return;
        }
        let scope = devices
            .drive
            .device
            .push_error_scope(wgpu::ErrorFilter::Validation);
        let mut runs = stream_frames(&[&devices.drive], 24);
        let (driven, _) = runs.pop().expect("driven run");
        let error = regia_core::tasks::block_on(scope.pop());
        assert!(error.is_none(), "validation error: {error:?}");
        let feedback = driven.residency_feedback_stats().expect("feedback on");
        assert_eq!(feedback.mode, FeedbackMode::Drive);
        assert!(feedback.readouts > 0, "{feedback:?}");
        assert_eq!(
            feedback.materials_driven, 1,
            "the streamed plane took the GPU need: {feedback:?}"
        );
    }

    /// Bench (ignored; run on a real GPU with `--ignored --nocapture`): the CPU cost of a table
    /// rebuild against the bound length. For each scene size it churns one texture per rebuild
    /// and prints microseconds per rebuild with the prefix bound (partially bound device) and with
    /// the whole table bound (the observe device, no partial binding). Prints only; nothing about
    /// time is asserted.
    #[test]
    #[ignore = "bench: cargo test -p regia_render --release table_rebuild_cost -- --ignored --nocapture"]
    fn table_rebuild_cost_by_bound_length() {
        let Some(devices) = devices() else {
            eprintln!("skipping: no adapter with the texture table features");
            return;
        };
        let Some(partial) = devices.partial.as_ref() else {
            eprintln!("skipping: no partially bound arrays on this adapter");
            return;
        };
        const REBUILDS: u32 = 50;
        println!("| textures | device | bound | us per rebuild | views written |");
        println!("|---|---|---|---|---|");
        for textures in [200usize, 900, 3000] {
            for (name, render) in [("partial", partial), ("full", &devices.observe)] {
                let device = &render.device;
                let reserved = [
                    solid(device, "white"),
                    solid(device, "flat"),
                    solid(device, "mid"),
                    solid(device, "aux"),
                ];
                let sampler = device.create_sampler(&wgpu::SamplerDescriptor::default());
                let Some(mut table) = TextureTable::new(
                    device,
                    [&reserved[0], &reserved[1], &reserved[2], &reserved[3]],
                    &sampler,
                    1,
                ) else {
                    continue;
                };
                if textures as u32 + RESERVED_INDICES > table.stats().capacity {
                    continue;
                }
                let set = |view: &wgpu::TextureView| {
                    [
                        view.clone(),
                        reserved[1].clone(),
                        reserved[0].clone(),
                        reserved[0].clone(),
                        reserved[2].clone(),
                        reserved[1].clone(),
                        reserved[3].clone(),
                    ]
                };
                let views: Vec<wgpu::TextureView> =
                    (0..textures).map(|i| solid(device, &format!("b{i}"))).collect();
                let mut held: Vec<_> = views.iter().map(|view| table.acquire_set(&set(view))).collect();
                table.refresh(device);
                let written_before = table.stats().views_written;
                let started = std::time::Instant::now();
                let top = &views[textures - 1];
                for _ in 0..REBUILDS {
                    // Churn the top texture (it leaves, then comes back at the same index): the
                    // table must rebuild, and the high-water mark stays where it was.
                    if let Some(last) = held.pop() {
                        table.release_set(last);
                    }
                    held.push(table.acquire_set(&set(top)));
                    table.refresh(device);
                }
                let per = started.elapsed().as_secs_f64() * 1e6 / f64::from(REBUILDS);
                let stats = table.stats();
                println!(
                    "| {textures} | {name} | {} | {per:.1} | {} |",
                    stats.bound,
                    stats.views_written - written_before
                );
            }
        }
    }
}
