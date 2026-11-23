// SPDX-License-Identifier: MPL-2.0
//! A minimal GPU timestamp timer (spec `26` § 6: VFX budget measured with GPU timestamps).
//!
//! Wraps one `QuerySet` of `TIMESTAMP_QUERY` slots plus resolve/readback buffers. Hosts write
//! pass timestamps (`ComputePassDescriptor::timestamp_writes` / `RenderPassDescriptor::
//! timestamp_writes`) or encoder timestamps, then call [`GpuTimer::read_ms`] after the frame.
//! The device must have been created with `wgpu::Features::TIMESTAMP_QUERY` —
//! [`regia_render::RenderDevice`] requests it whenever the adapter supports it.

/// One query set + readback path for GPU timing.
pub struct GpuTimer {
    set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    staging: wgpu::Buffer,
    slots: u32,
}

impl GpuTimer {
    /// Creates a timer with `slots` timestamp slots, or `None` when the device lacks
    /// `TIMESTAMP_QUERY`.
    pub fn new(device: &wgpu::Device, slots: u32) -> Option<Self> {
        if !device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            return None;
        }
        let set = device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("vfx timestamps"),
            ty: wgpu::QueryType::Timestamp,
            count: slots,
        });
        let resolve = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vfx timestamp resolve"),
            size: slots as u64 * 8,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let staging = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vfx timestamp staging"),
            size: slots as u64 * 8,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Some(Self {
            set,
            resolve,
            staging,
            slots,
        })
    }

    /// The query set (pass it to `timestamp_writes` or `encoder.write_timestamp`).
    pub fn query_set(&self) -> &wgpu::QuerySet {
        &self.set
    }

    /// Number of slots.
    pub fn slots(&self) -> u32 {
        self.slots
    }

    /// Records the resolve + copy for `used` slots onto `encoder` (call after the last
    /// timestamp write of the frame, before submitting).
    pub fn resolve(&self, encoder: &mut wgpu::CommandEncoder, used: u32) {
        let used = used.min(self.slots);
        encoder.resolve_query_set(&self.set, 0..used, &self.resolve, 0);
        encoder.copy_buffer_to_buffer(&self.resolve, 0, &self.staging, 0, used as u64 * 8);
    }

    /// Reads the slots back (blocking) and returns GPU-time milliseconds between
    /// timestamp pairs: `read_ms(device, queue)[i]` is the time between slot `2i` and slot
    /// `2i + 1`.
    ///
    /// Slots are consumed in pairs; write an even number of timestamps and call this after
    /// submitting the encoder that carried them.
    pub fn read_ms(&self, device: &wgpu::Device, queue: &wgpu::Queue) -> Option<Vec<f32>> {
        let (tx, rx) = std::sync::mpsc::channel();
        let slice = self.staging.slice(..);
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        let stamps = (|| {
            device.poll(wgpu::PollType::wait_indefinitely()).ok()?;
            rx.recv().ok()?.ok()?;
            let data = slice.get_mapped_range().ok()?;
            let stamps: Vec<u64> = data
                .as_chunks::<8>()
                .0
                .iter()
                .map(|c| u64::from_le_bytes(*c))
                .collect();
            Some(stamps)
        })();
        // Unmap on every path: a map left pending would panic the next `map_async`.
        self.staging.unmap();
        let stamps = stamps?;
        let period = queue.get_timestamp_period();
        Some(
            stamps
                .chunks(2)
                .filter(|c| c.len() == 2)
                .map(|c| c[1].saturating_sub(c[0]) as f32 * period / 1_000_000.0)
                .collect(),
        )
    }
}
