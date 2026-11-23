// SPDX-License-Identifier: MPL-2.0
//! The GPU half of `.vfx.ron` v6 event spawns.
//!
//! The update kernel stages every fired event at the deterministic rank
//! `slot_ordinal * EVENTS_PER_SLOT + n` (at most three events per pool slot per frame), stamps
//! it with the simulated step as an epoch, and marks the slot in a fired-slot mask. The next
//! frame `event_prepare` (one thread) walks the mask in slot order (clearing the words it
//! consumes, so the walk costs the slots that fired, not the window) and compacts the first
//! `event_capacity` fired events into the front of the same buffer — the keep-set is a pure
//! function of how many events fired and the slot order, never of thread scheduling, and never
//! of where in the window the firing slots sit: a frame that fires fewer events than the
//! budget keeps all of them (Muse review of fix-vfx1-determinism, finding 1). It then walks
//! the events in order reserving child-ring budget per link, rolling a link back when it does
//! not fit. `child_emit` expands each event at its reserved slots. The result: which children
//! spawn, and which ring slot each lands in, is identical on every run and every GPU while no
//! buffer overflows. Every draw comes from a hash of the parent's identity, the step and the
//! link ([`crate::events`], the CPU mirror), so the particle set and every count agree with
//! the CPU. The main ring's cursor stays a pure function of the host's spawn jobs, and a
//! scene without events records none of these dispatches and updates exactly the main ring's
//! window.

use bytemuck::Zeroable;

use super::VfxGpu;
use super::layout::{ChildInitGpu, EventGpu, EventLinkGpu, MAX_EMITTERS};
use super::shaders::{COMMON, EVENT_EMIT, EVENT_PREPARE};
use crate::format::MAX_EVENTS_PER_EMITTER;

/// `(name, source)` of the event prepare kernel, for the shader gate.
pub(super) fn prepare_source() -> (String, String) {
    (
        "vfx event prepare".to_string(),
        format!("{COMMON}{EVENT_PREPARE}"),
    )
}

/// `(name, source)` of the child-emit kernel, for the shader gate.
pub(super) fn emit_source() -> (String, String) {
    ("vfx child emit".to_string(), format!("{COMMON}{EVENT_EMIT}"))
}

/// The pool buffers the event kernels bind.
pub(super) struct EventBuffers<'a> {
    pub(super) events: &'a wgpu::Buffer,
    pub(super) pos_age: &'a wgpu::Buffer,
    pub(super) vel_life: &'a wgpu::Buffer,
    pub(super) attrib: &'a wgpu::Buffer,
    pub(super) color0: &'a wgpu::Buffer,
    pub(super) stats: &'a wgpu::Buffer,
    pub(super) event_epoch: &'a wgpu::Buffer,
    pub(super) event_mask: &'a wgpu::Buffer,
}

/// Event-spawn state of a [`VfxGpu`].
pub(super) struct EventPass {
    pub(super) event_capacity: u32,
    pub(super) child_capacity: u32,
    /// A live instance has events: the frame runs the event kernels and updates the child ring.
    pub(super) active: bool,
    /// Simulated steps so far; part of every event's seed and of every staged rank's epoch.
    pub(super) step: u32,
    links: wgpu::Buffer,
    child_inits: wgpu::Buffer,
    args: wgpu::Buffer,
    link_cpu: Vec<[EventLinkGpu; MAX_EVENTS_PER_EMITTER]>,
    init_cpu: Vec<ChildInitGpu>,
    /// Slots with a record to upload.
    dirty: Vec<bool>,
    /// Slots holding links or an init record.
    used: Vec<bool>,
    pipe_prepare: wgpu::ComputePipeline,
    pipe_emit: wgpu::ComputePipeline,
    bg_prepare: wgpu::BindGroup,
    bg_emit: wgpu::BindGroup,
}

impl EventPass {
    /// Builds the tables, pipelines and bind groups (inside the caller's error scope).
    pub(super) fn new(
        d: &wgpu::Device,
        bgl_frame: &wgpu::BindGroupLayout,
        buffers: &EventBuffers<'_>,
        event_capacity: u32,
        child_capacity: u32,
    ) -> Self {
        let table = |label: &str, size: u64, usage: wgpu::BufferUsages| {
            d.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage,
                mapped_at_creation: false,
            })
        };
        let storage = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST;
        let slots = u64::from(MAX_EMITTERS);
        let links = table(
            "vfx event links",
            slots * (MAX_EVENTS_PER_EMITTER * size_of::<EventLinkGpu>()) as u64,
            storage,
        );
        let child_inits = table(
            "vfx event child inits",
            slots * size_of::<ChildInitGpu>() as u64,
            storage,
        );
        // Held by the two bind groups below; the CPU never touches it again.
        let reservations = table(
            "vfx event reservations",
            u64::from(event_capacity) * 16,
            wgpu::BufferUsages::STORAGE,
        );
        let args = table(
            "vfx event dispatch args",
            12,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::INDIRECT | wgpu::BufferUsages::COPY_DST,
        );
        let cs = wgpu::ShaderStages::COMPUTE;
        let entry = |binding: u32, read_only: bool| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: cs,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        // Prepare: stats + args + events + links + reservations + event_epoch + event_mask.
        let bgl_prepare = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vfx event prepare bgl"),
            entries: &[
                entry(0, false),
                entry(1, false),
                entry(2, false),
                entry(3, true),
                entry(4, false),
                entry(5, false),
                // Read-write: the prepare clears each mask word it consumes.
                entry(6, false),
            ],
        });
        let bgl_emit = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vfx child emit bgl"),
            entries: &[
                entry(0, true),
                entry(1, true),
                entry(2, true),
                entry(3, false),
                entry(4, false),
                entry(5, false),
                entry(6, false),
                entry(7, false),
                entry(8, true),
            ],
        });
        fn bind(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
            wgpu::BindGroupEntry {
                binding,
                resource: buffer.as_entire_binding(),
            }
        }
        let bg_prepare = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vfx event prepare bg"),
            layout: &bgl_prepare,
            entries: &[
                bind(0, buffers.stats),
                bind(1, &args),
                bind(2, buffers.events),
                bind(3, &links),
                bind(4, &reservations),
                bind(5, buffers.event_epoch),
                bind(6, buffers.event_mask),
            ],
        });
        let bg_emit = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vfx child emit bg"),
            layout: &bgl_emit,
            entries: &[
                bind(0, buffers.events),
                bind(1, &links),
                bind(2, &child_inits),
                bind(3, buffers.pos_age),
                bind(4, buffers.vel_life),
                bind(5, buffers.attrib),
                bind(6, buffers.color0),
                bind(7, buffers.stats),
                bind(8, &reservations),
            ],
        });
        let pipeline = |label: &str, source: (String, String), group: &wgpu::BindGroupLayout, entry: &str| {
            let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(label),
                source: wgpu::ShaderSource::Wgsl(source.1.into()),
            });
            let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: &[Some(bgl_frame), Some(group)],
                immediate_size: 0,
            });
            d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                module: &module,
                entry_point: Some(entry),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                cache: regia_render::pipeline_cache::cache_for(d).as_deref(),
            })
        };
        let pipe_prepare = pipeline(
            "vfx event prepare",
            prepare_source(),
            &bgl_prepare,
            "event_prepare",
        );
        let pipe_emit = pipeline("vfx child emit", emit_source(), &bgl_emit, "child_emit");
        let slot_count = MAX_EMITTERS as usize;
        Self {
            event_capacity,
            child_capacity,
            active: false,
            step: 0,
            links,
            child_inits,
            args,
            link_cpu: vec![[EventLinkGpu::default(); MAX_EVENTS_PER_EMITTER]; slot_count],
            init_cpu: vec![ChildInitGpu::zeroed(); slot_count],
            dirty: vec![false; slot_count],
            used: vec![false; slot_count],
            pipe_prepare,
            pipe_emit,
            bg_prepare,
            bg_emit,
        }
    }

    /// Clears `n` slots from `start` (their instance retired).
    pub(super) fn free(&mut self, start: u32, n: u32) {
        for slot in start..start.saturating_add(n) {
            let slot = slot as usize;
            if self.used.get(slot).copied().unwrap_or(false) {
                self.link_cpu[slot] = [EventLinkGpu::default(); MAX_EVENTS_PER_EMITTER];
                self.init_cpu[slot] = ChildInitGpu::zeroed();
                self.used[slot] = false;
                self.dirty[slot] = true;
            }
        }
    }

    /// Uploads the staged records.
    pub(super) fn flush(&mut self, queue: &wgpu::Queue) {
        for (slot, dirty) in self.dirty.iter_mut().enumerate() {
            if !std::mem::take(dirty) {
                continue;
            }
            let link_size = (MAX_EVENTS_PER_EMITTER * size_of::<EventLinkGpu>()) as u64;
            queue.write_buffer(
                &self.links,
                slot as u64 * link_size,
                bytemuck::cast_slice(&self.link_cpu[slot]),
            );
            queue.write_buffer(
                &self.child_inits,
                slot as u64 * size_of::<ChildInitGpu>() as u64,
                bytemuck::bytes_of(&self.init_cpu[slot]),
            );
        }
    }

    /// The `FrameGpu::events` row for a frame stepping by `dt`.
    pub(super) fn frame_row(&mut self, dt: f32) -> [u32; 4] {
        if dt > 0.0 {
            self.step = self.step.wrapping_add(1);
        }
        [
            if self.active { self.child_capacity } else { 0 },
            self.step,
            self.event_capacity,
            self.child_capacity,
        ]
    }

    /// Records the dispatches that consume last frame's events.
    pub(super) fn record(&self, pass: &mut wgpu::ComputePass<'_>, bg_frame: &wgpu::BindGroup) {
        pass.set_pipeline(&self.pipe_prepare);
        pass.set_bind_group(0, Some(bg_frame), &[]);
        pass.set_bind_group(1, Some(&self.bg_prepare), &[]);
        pass.dispatch_workgroups(1, 1, 1);
        pass.set_pipeline(&self.pipe_emit);
        pass.set_bind_group(0, Some(bg_frame), &[]);
        pass.set_bind_group(1, Some(&self.bg_emit), &[]);
        pass.dispatch_workgroups_indirect(&self.args, 0);
    }
}

/// One particle of the pool, read back ([`VfxGpu::read_particles`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PoolParticle {
    /// Pool slot. Slots from [`VfxGpu::capacity`] on are the child ring.
    pub slot: u32,
    /// Its emitter-record slot.
    pub emitter_slot: u32,
    /// World position.
    pub position: [f32; 3],
    /// Velocity.
    pub velocity: [f32; 3],
    /// Age, seconds.
    pub age: f32,
    /// Lifetime, seconds.
    pub life: f32,
    /// Base size, metres.
    pub size: f32,
}

impl VfxGpu {
    /// Whether a live instance has event spawns; [`crate::scene::VfxScene::upload`] sets it every
    /// frame. Off, the frame records no event dispatch and updates only the main ring.
    pub fn set_events_active(&mut self, active: bool) {
        self.event_gpu.active = active;
    }

    /// Whether event spawns run this frame ([`Self::set_events_active`]).
    pub fn events_active(&self) -> bool {
        self.event_gpu.active
    }

    /// Slots of the child ring ([`super::VfxProfile::child_capacity`] after clamping).
    pub fn child_capacity(&self) -> u32 {
        self.event_gpu.child_capacity
    }

    /// Events one frame can fire before the rest are lost
    /// ([`super::VfxProfile::event_capacity`] after clamping).
    pub fn event_capacity(&self) -> u32 {
        self.event_gpu.event_capacity
    }

    /// Stages the event links of emitter record `slot`, and the init record its particles start
    /// from when it is a child (uploaded by [`Self::set_frame`]). Out-of-range slots are ignored.
    pub fn write_emitter_events(
        &mut self,
        slot: u32,
        links: [EventLinkGpu; MAX_EVENTS_PER_EMITTER],
        init: ChildInitGpu,
    ) {
        let index = slot as usize;
        let events = &mut self.event_gpu;
        if index >= events.dirty.len() {
            return;
        }
        if events.link_cpu[index] != links || events.init_cpu[index] != init {
            events.link_cpu[index] = links;
            events.init_cpu[index] = init;
            events.dirty[index] = true;
        }
        events.used[index] = true;
    }

    /// [`Self::read_pool_stats`] of the pool as it stands now, without a recorded copy (blocks on
    /// the device; the agent surface's `vfx.systems`).
    pub fn read_pool_stats_now(&self) -> Option<super::VfxPoolStats> {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        self.record_stats_copy(&mut encoder);
        self.queue.submit([encoder.finish()]);
        self.read_pool_stats()
    }

    /// Reads every live particle of the pool back, the child ring's included (blocks; tests and
    /// the agent surface's diagnostics).
    pub fn read_particles(&self) -> Vec<PoolParticle> {
        let (pos_age, vel_life, attrib) = (
            self.read_pool_words(&self.sim.pos_age),
            self.read_pool_words(&self.sim.vel_life),
            self.read_pool_words(&self.sim.attrib),
        );
        pos_age
            .iter()
            .zip(&vel_life)
            .zip(&attrib)
            .enumerate()
            .filter(|(_, ((p, v), _))| v[3] > 0.0 && p[3] < v[3])
            .map(|(slot, ((p, v), a))| PoolParticle {
                slot: slot as u32,
                emitter_slot: a[3] as u32,
                position: [p[0], p[1], p[2]],
                velocity: [v[0], v[1], v[2]],
                age: p[3],
                life: v[3],
                size: a[0],
            })
            .collect()
    }

    /// Copies one pool buffer out as `vec4`s (blocks). Empty when the map fails.
    fn read_pool_words(&self, buffer: &wgpu::Buffer) -> Vec<[f32; 4]> {
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vfx pool readback"),
            size: buffer.size(),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_buffer_to_buffer(buffer, 0, &staging, 0, buffer.size());
        self.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        let slice = staging.slice(..);
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let out = (|| {
            self.device.poll(wgpu::PollType::wait_indefinitely()).ok()?;
            rx.recv().ok()?.ok()?;
            let data = slice.get_mapped_range().ok()?;
            Some(bytemuck::cast_slice::<u8, [f32; 4]>(&data[..]).to_vec())
        })();
        staging.unmap();
        out.unwrap_or_default()
    }
}

// `EventGpu` is the WGSL `VfxEvent`: the buffer's stride, kept next to the kernels that use it.
const _: () = assert!(size_of::<EventGpu>() == 32);

#[cfg(test)]
mod tests {
    use super::{emit_source, prepare_source};

    #[test]
    fn event_kernels_have_no_function_layout_errors() {
        for (name, source) in [prepare_source(), emit_source()] {
            let errors =
                regia_render::wgsl_function_layout_errors(&name, &source).expect("event kernel emits SPIR-V");
            assert!(errors.is_empty(), "{name}: {errors:?}");
        }
    }

    /// The event kernels pass `spirv-val --target-env vulkan1.3` and create cleanly on a
    /// validating device (the prepare kernel now writes the mask it scans: rev-kimi-19
    /// finding 1). `cargo xtask check-shaders` runs the same check over every VFX module.
    #[test]
    fn event_kernels_pass_spirv_val() {
        let (prepare_name, prepare) = prepare_source();
        let (emit_name, emit) = emit_source();
        if let Err(err) =
            regia_render::validate_wgsl_modules(&[(&prepare_name, &prepare), (&emit_name, &emit)])
        {
            panic!("event kernels fail validation: {err}");
        }
    }
}
