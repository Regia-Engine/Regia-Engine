// SPDX-License-Identifier: MPL-2.0
//! Bounded upload staging during level loads (ADR-1346).
//!
//! `queue.write_buffer` and `queue.write_texture` copy their bytes into a new host-visible
//! staging buffer, which wgpu keeps until the next `queue.submit` and frees once the GPU has run
//! that submit. The first frame of a level uploads every new material texture and mesh, so with
//! one submit per frame the whole level's texture bytes sat in staging at once, on top of the CPU
//! copies the materials hold: the DO27 map pak (4.9 GiB of textures) peaked at about 9 GiB of RSS
//! (`docs/evidence/perf/zero-copy-load-v020`).
//!
//! The renderer's tracked queue counts the bytes written on this thread since its last submit.
//! When they reach the cap ([`DEFAULT_STAGING_FLUSH_CAP`], `REGIA_STAGING_FLUSH_MIB`,
//! [`set_staging_flush_cap`]), it submits an empty batch: wgpu turns the pending writes into a
//! submission of their own. That submission runs before every later one, and a write was always
//! applied before the next submit's command buffers anyway, so every pass reads the same bytes
//! and the frame draws the same pixels. When the queue knows its device (the frame path's
//! queue does), the flush then waits for the *previous* flush to finish, so at most two
//! flushes' staging is alive (one the GPU copies while the CPU fills the next): the load's
//! staging peak is about twice the cap plus one write, instead of the level's size.
//!
//! A steady frame writes far less than the cap and never flushes. [`staging_flush_stats`]
//! reports the flushes, their bytes, the waits and the largest staging that was ever pending
//! at once; `seeing.memory_report` returns them as `staging`.

// The thread-local initializers below are already `const`; clippy's `missing_const_for_thread_local`
// still fires on aarch64-linux-android (no native TLS there), as in `frame_stats` (ADR-1346).
#![cfg_attr(target_os = "android", allow(clippy::missing_const_for_thread_local))]

use std::cell::Cell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Default cap: bytes written between submits before the pending writes are flushed (128 MiB).
pub const DEFAULT_STAGING_FLUSH_CAP: u64 = 128 << 20;

/// Environment variable: the cap in MiB; `0` or `off` turns the flush off (one staging copy of
/// everything a frame writes, the behavior before ADR-1346).
pub const STAGING_FLUSH_ENV: &str = "REGIA_STAGING_FLUSH_MIB";

/// The longest a flush waits for the flush before it. A device that takes longer is lost or
/// badly behind; the load then goes on without the bound rather than hang.
pub(crate) const FLUSH_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// [`set_staging_flush_cap`]'s value; `u64::MAX` = not set (the environment or the default).
static CAP_OVERRIDE: AtomicU64 = AtomicU64::new(u64::MAX);

// Process totals behind [`staging_flush_stats`]: flushes, flushed bytes, waits, peak pending.
static TOTALS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

// Per thread: the writes since this thread's last submit (the queue's pending staging).
thread_local! {
    static PENDING: Cell<u64> = const { Cell::new(0) };
}

// Tests only: this thread's counters and a cap that leaves other test threads alone.
#[cfg(test)]
thread_local! {
    static THREAD: Cell<ThreadCounts> = const { Cell::new(ThreadCounts::ZERO) };
    static THREAD_CAP: Cell<Option<u64>> = const { Cell::new(None) };
}

/// This thread's flush counters (tests read deltas of these; other threads cannot move them).
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ThreadCounts {
    pub(crate) flushes: u64,
    pub(crate) flushed_bytes: u64,
    pub(crate) waits: u64,
    pub(crate) peak_pending_bytes: u64,
    pub(crate) largest_write_bytes: u64,
}

#[cfg(test)]
impl ThreadCounts {
    const ZERO: Self = Self {
        flushes: 0,
        flushed_bytes: 0,
        waits: 0,
        peak_pending_bytes: 0,
        largest_write_bytes: 0,
    };
}

/// The staging flush since start, every thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StagingFlushStats {
    /// The cap in force now, bytes; `0` = the flush is off.
    pub cap_bytes: u64,
    /// Flushes: empty submits that moved pending writes into a submission of their own.
    pub flushes: u64,
    /// Bytes those flushes moved.
    pub flushed_bytes: u64,
    /// Flushes that waited for the flush before them (the queue knew its device).
    pub waits: u64,
    /// The most bytes ever written on one thread between two submits: the largest staging
    /// wgpu held at once for that thread's writes (at most the cap plus the last write while
    /// the flush is on).
    pub peak_pending_bytes: u64,
}

/// The cap a `REGIA_STAGING_FLUSH_MIB` value means: `None` (unset or not a number) is the
/// default, `0` / `off` / `false` turns the flush off, a number is MiB.
#[must_use]
pub(crate) fn cap_from_env_value(value: Option<&str>) -> u64 {
    match value.map(str::trim) {
        Some("off" | "false") => 0,
        Some(v) => v
            .parse::<u64>()
            .map_or(DEFAULT_STAGING_FLUSH_CAP, |mib| mib.saturating_mul(1 << 20)),
        None => DEFAULT_STAGING_FLUSH_CAP,
    }
}

fn env_cap() -> u64 {
    static CAP: OnceLock<u64> = OnceLock::new();
    *CAP.get_or_init(|| cap_from_env_value(std::env::var(STAGING_FLUSH_ENV).ok().as_deref()))
}

/// Sets the staging cap for every thread: `Some(bytes)` (`Some(0)` turns the flush off), or
/// `None` to go back to `REGIA_STAGING_FLUSH_MIB` or [`DEFAULT_STAGING_FLUSH_CAP`]. A host on
/// a memory-tight device (a handheld, a phone) can lower it before a level load; each flush is
/// one empty submit and, on the frame path, one wait for the flush before it.
pub fn set_staging_flush_cap(bytes: Option<u64>) {
    CAP_OVERRIDE.store(bytes.map_or(u64::MAX, |b| b.min(u64::MAX - 1)), Ordering::Relaxed);
}

/// The cap in force on this thread, bytes (`0` = off).
#[must_use]
pub(crate) fn cap() -> u64 {
    #[cfg(test)]
    {
        if let Some(cap) = THREAD_CAP.with(Cell::get) {
            return cap;
        }
    }
    match CAP_OVERRIDE.load(Ordering::Relaxed) {
        u64::MAX => env_cap(),
        cap => cap,
    }
}

/// Test hook: a cap for this thread only (`None` = the process cap again).
#[cfg(test)]
pub(crate) fn set_thread_cap(bytes: Option<u64>) {
    THREAD_CAP.with(|cap| cap.set(bytes));
}

/// `bytes` were written through the queue on this thread. `true` when the pending writes have
/// reached the cap and the caller must flush them ([`take_pending`], then [`note_flush`]).
#[must_use]
pub(crate) fn note_write(bytes: u64) -> bool {
    let pending = PENDING.with(|p| {
        let next = p.get().saturating_add(bytes);
        p.set(next);
        next
    });
    #[cfg(test)]
    THREAD.with(|t| {
        let mut counts = t.get();
        counts.peak_pending_bytes = counts.peak_pending_bytes.max(pending);
        counts.largest_write_bytes = counts.largest_write_bytes.max(bytes);
        t.set(counts);
    });
    TOTALS[3].fetch_max(pending, Ordering::Relaxed);
    let cap = cap();
    cap > 0 && pending >= cap
}

/// A submit on this thread took every pending write with it.
pub(crate) fn note_submit() {
    PENDING.with(|p| p.set(0));
}

/// The pending bytes a flush is about to move; this thread's count starts again at zero.
#[must_use]
pub(crate) fn take_pending() -> u64 {
    PENDING.with(|p| p.replace(0))
}

/// A flush moved `bytes`; `waited` when it waited for the flush before it.
pub(crate) fn note_flush(bytes: u64, waited: bool) {
    #[cfg(test)]
    THREAD.with(|t| {
        let mut counts = t.get();
        counts.flushes += 1;
        counts.flushed_bytes = counts.flushed_bytes.saturating_add(bytes);
        counts.waits += u64::from(waited);
        t.set(counts);
    });
    TOTALS[0].fetch_add(1, Ordering::Relaxed);
    TOTALS[1].fetch_add(bytes, Ordering::Relaxed);
    TOTALS[2].fetch_add(u64::from(waited), Ordering::Relaxed);
}

/// This thread's counters.
#[cfg(test)]
#[must_use]
pub(crate) fn thread_counts() -> ThreadCounts {
    THREAD.with(Cell::get)
}

/// The staging flush since start, every thread (the cap is this thread's).
#[must_use]
pub fn staging_flush_stats() -> StagingFlushStats {
    let at = |i: usize| TOTALS[i].load(Ordering::Relaxed);
    StagingFlushStats {
        cap_bytes: cap(),
        flushes: at(0),
        flushed_bytes: at(1),
        waits: at(2),
        peak_pending_bytes: at(3),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `body` with this thread's cap set to `cap`, starting from no pending writes and
    /// zeroed thread counters.
    fn with_cap<T>(cap: u64, body: impl FnOnce() -> T) -> T {
        set_thread_cap(Some(cap));
        note_submit();
        THREAD.with(|t| t.set(ThreadCounts::ZERO));
        let out = body();
        note_submit();
        set_thread_cap(None);
        out
    }

    #[test]
    fn env_values_parse_to_a_cap() {
        assert_eq!(cap_from_env_value(None), DEFAULT_STAGING_FLUSH_CAP);
        assert_eq!(cap_from_env_value(Some("0")), 0);
        assert_eq!(cap_from_env_value(Some("off")), 0);
        assert_eq!(cap_from_env_value(Some(" 64 ")), 64 << 20);
        assert_eq!(cap_from_env_value(Some("lots")), DEFAULT_STAGING_FLUSH_CAP);
        assert_eq!(cap_from_env_value(Some("18446744073709551615")), u64::MAX);
    }

    #[test]
    fn writes_below_the_cap_never_flush_and_a_submit_resets_them() {
        with_cap(100, || {
            assert!(!note_write(40));
            assert!(!note_write(59));
            note_submit();
            // 99 more after the submit: still under the cap.
            assert!(!note_write(99));
        });
    }

    #[test]
    fn the_write_that_reaches_the_cap_asks_for_a_flush() {
        with_cap(100, || {
            assert!(!note_write(60));
            assert!(note_write(40), "exactly the cap flushes");
            assert_eq!(take_pending(), 100);
            note_flush(100, false);
            // A single write larger than the cap flushes on its own.
            assert!(note_write(250));
            assert_eq!(take_pending(), 250);
            note_flush(250, true);
            assert_eq!(
                thread_counts(),
                ThreadCounts {
                    flushes: 2,
                    flushed_bytes: 350,
                    waits: 1,
                    peak_pending_bytes: 250,
                    largest_write_bytes: 250,
                }
            );
            assert!(staging_flush_stats().flushes >= 2, "the process totals saw them");
        });
    }

    #[test]
    fn a_zero_cap_is_off() {
        with_cap(0, || {
            for _ in 0..1000 {
                assert!(!note_write(1 << 20));
            }
            assert_eq!(cap(), 0);
        });
    }

    #[test]
    fn the_thread_cap_wins_over_the_process_cap() {
        with_cap(7, || assert_eq!(cap(), 7));
        assert_ne!(cap(), 7, "the test cap is gone afterwards");
    }

    #[test]
    fn a_host_sets_the_process_cap_and_clears_it() {
        let before = cap();
        // A cap no test frame reaches, so the other test threads run as before meanwhile.
        set_staging_flush_cap(Some(1 << 40));
        assert_eq!(cap(), 1 << 40);
        assert_eq!(staging_flush_stats().cap_bytes, 1 << 40);
        set_staging_flush_cap(None);
        assert_eq!(cap(), before);
    }

    /// An RGBA8 `side`² texture the test can copy from.
    fn texture(device: &wgpu::Device, side: u32) -> wgpu::Texture {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some("staging flush test"),
            size: wgpu::Extent3d {
                width: side,
                height: side,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        })
    }

    /// Eighteen 64 KiB texture writes through the frame path's queue handle with a cap of four
    /// writes: the queue flushes after writes 4, 8, 12 and 16 (the last two go with the
    /// test's own submit), every flush after the first waits for the one before, no more than
    /// the cap was ever pending, and every texture holds its own bytes afterwards: a write
    /// moved into a flush lands exactly as one left for the next submit.
    #[test]
    fn flushed_writes_land_and_bound_the_pending_staging() {
        let device = crate::device::test_device();
        let queue = device.tracked_queue();
        let side = 128u32;
        let level = u64::from(side * side * 4);
        let count = 18u8;
        with_cap(4 * level, || {
            let scope = device.device.push_error_scope(wgpu::ErrorFilter::Validation);
            let textures: Vec<wgpu::Texture> = (0..count)
                .map(|i| {
                    let t = texture(&device.device, side);
                    queue.write_texture(
                        t.as_image_copy(),
                        &vec![i.wrapping_mul(13).wrapping_add(1); level as usize],
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(side * 4),
                            rows_per_image: Some(side),
                        },
                        wgpu::Extent3d {
                            width: side,
                            height: side,
                            depth_or_array_layers: 1,
                        },
                    );
                    t
                })
                .collect();
            let counts = thread_counts();
            // Flushes 2-4 wait for the one before; the first waits too when another test on the
            // shared device flushed before it (the last-flush slot is the device's).
            assert!((3..=4).contains(&counts.waits), "{counts:?}");
            assert_eq!(
                counts,
                ThreadCounts {
                    flushes: 4,
                    flushed_bytes: 16 * level,
                    waits: counts.waits,
                    peak_pending_bytes: 4 * level,
                    largest_write_bytes: level,
                }
            );

            // Texel row 0 of every texture into one readback buffer, 256 bytes apart.
            let readback = device.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("staging flush readback"),
                size: 256 * u64::from(count),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let mut encoder = device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            for (i, t) in textures.iter().enumerate() {
                encoder.copy_texture_to_buffer(
                    t.as_image_copy(),
                    wgpu::TexelCopyBufferInfo {
                        buffer: &readback,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 256 * i as u64,
                            bytes_per_row: Some(256),
                            rows_per_image: Some(1),
                        },
                    },
                    wgpu::Extent3d {
                        width: 64,
                        height: 1,
                        depth_or_array_layers: 1,
                    },
                );
            }
            queue.submit([encoder.finish()]);
            readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
            if let Err(err) = device.device.poll(wgpu::PollType::wait_indefinitely()) {
                panic!("poll: {err}");
            }
            let error = regia_core::tasks::block_on(scope.pop());
            assert!(error.is_none(), "validation error: {error:?}");
            let bytes = readback.slice(..).get_mapped_range().expect("mapped").to_vec();
            for i in 0..count {
                let row = &bytes[256 * usize::from(i)..256 * usize::from(i) + 256];
                let want = i.wrapping_mul(13).wrapping_add(1);
                assert!(row.iter().all(|&b| b == want), "texture {i} holds its own bytes");
            }
        });
    }

    /// A handle without its device ([`crate::device::TrackedQueue::observe`]) still flushes at
    /// the cap; it only cannot wait.
    #[test]
    fn a_queue_without_its_device_flushes_without_waiting() {
        let device = crate::device::test_device();
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("staging flush test"),
            size: 4096,
            usage: wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        with_cap(4096, || {
            for _ in 0..3 {
                queue.write_buffer(&buffer, 0, &[7u8; 4096]);
            }
            let counts = thread_counts();
            assert_eq!((counts.flushes, counts.waits), (3, 0));
        });
    }

    /// The first frame of a textured scene (six materials with mipped textures, meshes,
    /// uniforms) draws the same pixels with a flush after every single write (and a wait for
    /// the flush before it) as with the flush off: moving pending writes into submissions of
    /// their own changes no pass's input.
    #[test]
    fn a_frame_flushed_at_every_write_draws_the_same_pixels() {
        let device = crate::device::test_device();
        let material = |i: u8| crate::LabMaterial {
            albedo: Some(crate::LabTexture {
                width: 32,
                height: 32,
                rgba: [40 * i, 128, 255 - 40 * i, 255].repeat(32 * 32),
            }),
            ..crate::LabMaterial::default()
        };
        let quad = |i: u8| {
            let x = f32::from(i) * 0.4 - 1.0;
            crate::LabMesh {
                vertices: vec![
                    crate::LabVertex::from_pos_normal_uv([x - 0.15, -0.15, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0]),
                    crate::LabVertex::from_pos_normal_uv([x + 0.15, -0.15, 0.0], [0.0, 0.0, 1.0], [1.0, 1.0]),
                    crate::LabVertex::from_pos_normal_uv([x + 0.15, 0.15, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0]),
                    crate::LabVertex::from_pos_normal_uv([x - 0.15, 0.15, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
                ],
                indices: vec![0, 1, 2, 0, 2, 3],
                material: usize::from(i),
                ..crate::LabMesh::unskinned()
            }
        };
        let lab = crate::LabScene {
            width: 48,
            height: 32,
            eye: [0.0, 0.0, 3.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 0.9,
            near: 0.1,
            far: 10.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: false,
            exposure_ev: 12.0,
            ambient_lux: 5_000.0,
            double_sided: false,
            post: None,
            directional: crate::DirectionalLightUniform::new([0.0, -0.3, -1.0], [1.0; 3], 20_000.0),
            local_lights: Vec::new(),
            materials: (0..6).map(material).collect(),
            meshes: (0..6).map(quad).collect(),
        };
        let frame = lab.as_frame_scene();
        let render = |cap: u64| {
            with_cap(cap, || {
                let mut renderer = crate::FrameRenderer::new();
                let image = match renderer.render_offscreen(&device, &frame) {
                    Ok(image) => image,
                    Err(err) => panic!("frame: {err}"),
                };
                (image.pixels, thread_counts())
            })
        };
        let (off, off_counts) = render(0);
        let (flushed, counts) = render(1);
        assert_eq!(off_counts.flushes, 0, "a zero cap never flushes");
        assert!(counts.flushes >= 6, "every texture write flushed: {counts:?}");
        // The frame's own handle knows its device and waits; set-up writes through device-less
        // handles flush without waiting.
        assert!(counts.waits > 0, "the frame path's queue waits: {counts:?}");
        assert!(off.iter().any(|&b| b != 0), "the frame drew something");
        assert!(off == flushed, "flushing changed the frame's pixels");
    }

    /// A level-sized first frame: sixteen materials with distinct 512² RGBA8 base colors (about
    /// 21 MiB of texture writes with their mips). With the flush off, the frame keeps the whole
    /// level pending in staging until its submit; at a 2 MiB cap no more than the cap plus one
    /// write is ever pending, and the frame flushes several times.
    #[test]
    fn a_level_frame_keeps_at_most_a_cap_of_staging_pending() {
        let device = crate::device::test_device();
        let side = 512u32;
        let material = |i: u8| crate::LabMaterial {
            albedo: Some(crate::LabTexture {
                width: side,
                height: side,
                rgba: [i.wrapping_mul(29), 90, 255 - i.wrapping_mul(13), 255].repeat((side * side) as usize),
            }),
            ..crate::LabMaterial::default()
        };
        let quad = |i: u8| {
            let (x, y) = (f32::from(i % 4) * 0.6 - 0.9, f32::from(i / 4) * 0.6 - 0.9);
            let n = [0.0, 0.0, 1.0];
            crate::LabMesh {
                vertices: vec![
                    crate::LabVertex::from_pos_normal_uv([x - 0.25, y - 0.25, 0.0], n, [0.0, 1.0]),
                    crate::LabVertex::from_pos_normal_uv([x + 0.25, y - 0.25, 0.0], n, [1.0, 1.0]),
                    crate::LabVertex::from_pos_normal_uv([x + 0.25, y + 0.25, 0.0], n, [1.0, 0.0]),
                    crate::LabVertex::from_pos_normal_uv([x - 0.25, y + 0.25, 0.0], n, [0.0, 0.0]),
                ],
                indices: vec![0, 1, 2, 0, 2, 3],
                material: usize::from(i),
                ..crate::LabMesh::unskinned()
            }
        };
        let lab = crate::LabScene {
            width: 64,
            height: 64,
            eye: [0.0, 0.0, 3.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 0.9,
            near: 0.1,
            far: 10.0,
            depth_prepass: false,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 12.0,
            ambient_lux: 5_000.0,
            double_sided: false,
            post: None,
            directional: crate::DirectionalLightUniform::new([0.0, -0.3, -1.0], [1.0; 3], 20_000.0),
            local_lights: Vec::new(),
            materials: (0..16).map(material).collect(),
            meshes: (0..16).map(quad).collect(),
        };
        let frame = lab.as_frame_scene();
        let level_bytes = 16 * u64::from(side * side * 4);
        let cap = 2 << 20;
        let render = |cap: u64| {
            with_cap(cap, || {
                let mut renderer = crate::FrameRenderer::new();
                if let Err(err) = renderer.render_offscreen(&device, &frame) {
                    panic!("frame: {err}");
                }
                thread_counts()
            })
        };
        let off = render(0);
        let on = render(cap);
        assert!(
            off.peak_pending_bytes >= level_bytes,
            "with the flush off every level-0 texel was pending at once: {off:?}"
        );
        assert!(
            on.peak_pending_bytes <= cap + on.largest_write_bytes,
            "at most the cap plus one write is pending: {on:?}"
        );
        assert!(
            on.peak_pending_bytes < off.peak_pending_bytes / 2,
            "the flush bounds the frame's staging: {on:?} vs {off:?}"
        );
        // Each flush moves less than the cap plus one write, and less than the cap is left for
        // the frame's own submit.
        assert!(
            on.flushes >= (level_bytes - cap) / (cap + on.largest_write_bytes),
            "{on:?}"
        );
        assert!(
            on.waits > 0,
            "the frame path waited for its previous flush: {on:?}"
        );
    }
}
