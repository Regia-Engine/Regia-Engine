// SPDX-License-Identifier: MIT-0
//! Memory budgets on DO27's content (`mem_report`): level load time (cold and warm cache),
//! bytes read, process RSS and the renderer's GPU memory by category
//! ([`regia_render::memory_report`]) for one map at the tier `LOOK_R2_TIER` selects, then a
//! streaming soak (the chase from a cold start) that counts frames over 2× p50.
//!
//! The level load is everything from the first content read to the first frame finished on
//! the GPU: the content decode (`R2Planets::get`: heightfield, layers, cooked pack, set
//! pieces, scatter), the GPU uploads and pipeline builds of the runtime frame, and frame 0.
//! Storage estimates add the bytes read at a device's sequential rate to the measured warm
//! load (the decode is CPU-bound and does not overlap the reads in this loader), so they are
//! upper bounds for that device.

use std::path::Path;
use std::time::Instant;

use regia_render::{MemoryCategory, MemoryReport, RenderDevice};

use crate::chase::{ChaseOptions, ChaseRun, percentile, run_with};
use crate::look_r2::R2Planets;
use crate::{LookdevError, Planet};

/// NVMe sequential read assumed for the cold-load estimate, bytes/s (PCIe 3 class).
pub const NVME_BYTES_PER_S: f64 = 2.0e9;
/// Steam Deck class storage (its eMMC/NVMe SKUs and microSD span ~0.1–2.5 GB/s; the brief
/// sets 1 GB/s), bytes/s.
pub const DECK_BYTES_PER_S: f64 = 1.0e9;
/// The Deck's Zen 2 core against the farm's for the CPU part of the load (single-thread
/// PNG/JSON decode): a conservative factor.
pub const DECK_CPU_FACTOR: f64 = 2.0;

/// Budgets the lane sets (`docs/evidence/perf/memory-20261007.md`).
pub mod budget {
    /// Desktop High VRAM, bytes.
    pub const DESKTOP_HIGH_VRAM: u64 = 6 << 30;
    /// Deck tier VRAM, bytes.
    pub const DECK_VRAM: u64 = 3 << 30;
    /// Deck tier process RSS, bytes.
    pub const DECK_RSS: u64 = 5 << 30;
    /// Level load, cold, on an NVMe, seconds.
    pub const LOAD_NVME_S: f64 = 10.0;
    /// Level load, cold, on Deck-class storage, seconds.
    pub const LOAD_DECK_S: f64 = 25.0;
}

/// `/proc/self/io` counters (Linux): bytes passed to `read` and bytes fetched from storage.
#[derive(Debug, Clone, Copy, Default)]
pub struct IoCounters {
    /// `rchar`: bytes read through syscalls (page cache hits included).
    pub rchar: u64,
    /// `read_bytes`: bytes the block layer fetched for this process.
    pub read_bytes: u64,
}

impl IoCounters {
    /// The current counters (zeros where `/proc/self/io` is missing).
    #[must_use]
    pub fn now() -> Self {
        let text = std::fs::read_to_string("/proc/self/io").unwrap_or_default();
        let get = |key: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(key))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0)
        };
        Self {
            rchar: get("rchar:"),
            read_bytes: get("read_bytes:"),
        }
    }

    /// Counters since `earlier`.
    #[must_use]
    pub fn since(self, earlier: Self) -> Self {
        Self {
            rchar: self.rchar.saturating_sub(earlier.rchar),
            read_bytes: self.read_bytes.saturating_sub(earlier.read_bytes),
        }
    }
}

/// Drops `dir`'s files from the page cache (`dd iflag=nocache count=0` per file:
/// `posix_fadvise(DONTNEED)`, no root needed for clean pages). Returns the bytes covered.
///
/// # Errors
///
/// The directory cannot be walked.
pub fn evict_page_cache(dir: &Path) -> Result<u64, LookdevError> {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = std::fs::read_dir(&d).map_err(|source| LookdevError::Io {
            path: d.display().to_string(),
            source,
        })?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
                let _ = std::process::Command::new("dd")
                    .arg(format!("if={}", path.display()))
                    .args(["iflag=nocache", "count=0", "status=none"])
                    .status();
            }
        }
    }
    Ok(total)
}

/// Bytes of every file under `dir`.
#[must_use]
pub fn dir_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

/// One map's measurement.
#[derive(Debug, Clone)]
pub struct MemoryRun {
    /// Map name.
    pub map: &'static str,
    /// Tier name (`LOOK_R2_TIER`, empty = Desktop High).
    pub tier: String,
    /// Content decode (`R2Planets::get`), seconds.
    pub decode_s: f64,
    /// Content decode to the first frame finished on the GPU, seconds.
    pub load_s: f64,
    /// Bytes read during the load.
    pub io: IoCounters,
    /// The map directory's size on disk (what a pak of this loader's inputs holds), bytes.
    pub content_bytes: u64,
    /// GPU memory after the warm-up (steady state at the start pose).
    pub steady: MemoryReport,
    /// GPU memory after the drive.
    pub after_drive: MemoryReport,
    /// Peak RSS over the run, bytes.
    pub peak_rss: Option<u64>,
    /// Texture streaming after the drive.
    pub stream: regia_render::StreamReport,
    /// Every material texture the renderer holds after the drive, and the budget.
    pub textures: regia_render::TextureMemoryReport,
    /// The renderer's frame problems after the drive.
    pub problems: Vec<String>,
    /// RGBA8 bytes the host's materials hold (the decoded images it handed the renderer).
    pub host_texture_bytes: u64,
    /// Of the host's images, how many are RGBA8 (not cooked block-compressed chains).
    pub host_uncompressed: usize,
    /// Bytes streamed per frame (warm-up included).
    pub streamed_bytes: Vec<u64>,
    /// Sets swapped in per frame (warm-up included).
    pub swapped: Vec<u32>,
    /// Stream pass CPU time per frame, µs (warm-up included).
    pub stream_us: Vec<u32>,
    /// First frame with every streamed material settled (resident at its cap, no fade).
    pub settled_frame: Option<u32>,
    /// The chase samples.
    pub chase: ChaseRun,
}

impl MemoryRun {
    /// Cold load on an NVMe (warm load + bytes at [`NVME_BYTES_PER_S`]), seconds.
    #[must_use]
    pub fn nvme_estimate_s(&self) -> f64 {
        self.load_s + self.io.rchar as f64 / NVME_BYTES_PER_S
    }

    /// Cold load on Deck-class storage and CPU, seconds.
    #[must_use]
    pub fn deck_estimate_s(&self) -> f64 {
        self.load_s * DECK_CPU_FACTOR + self.io.rchar as f64 / DECK_BYTES_PER_S
    }

    /// Frames over 2× the p50 wall time, with their index and wall ms.
    #[must_use]
    pub fn hitches(&self) -> Vec<(usize, f32)> {
        let walls: Vec<f32> = self.chase.samples.iter().map(|s| s.wall_ms).collect();
        let p50 = percentile(&walls, 0.5);
        walls
            .iter()
            .enumerate()
            .filter(|(_, w)| **w > 2.0 * p50)
            .map(|(i, w)| (i, *w))
            .collect()
    }

    /// The summary block the evidence file quotes.
    #[must_use]
    pub fn summary(&self) -> String {
        use std::fmt::Write as _;
        let gib = |b: u64| b as f64 / f64::from(1u32 << 30);
        let mib = |b: u64| b as f64 / f64::from(1u32 << 20);
        let mut out = String::new();
        let tier = if self.tier.is_empty() {
            "desktop-high"
        } else {
            &self.tier
        };
        let _ = writeln!(out, "== {} @ {tier}", self.map);
        let _ = writeln!(
            out,
            "load: decode {:.2} s, to first frame {:.2} s; read {:.1} MiB (storage {:.1} MiB); content dir {:.1} MiB",
            self.decode_s,
            self.load_s,
            mib(self.io.rchar),
            mib(self.io.read_bytes),
            mib(self.content_bytes)
        );
        let _ = writeln!(
            out,
            "cold estimates: NVMe {:.1} s (budget {:.0}), Deck {:.1} s (budget {:.0})",
            self.nvme_estimate_s(),
            budget::LOAD_NVME_S,
            self.deck_estimate_s(),
            budget::LOAD_DECK_S
        );
        let _ = writeln!(
            out,
            "GPU {:.2} GiB steady, {:.2} GiB after drive; RSS {:.2} GiB, peak {:.2} GiB",
            gib(self.steady.allocated_bytes),
            gib(self.after_drive.allocated_bytes),
            gib(self.after_drive.rss_bytes.unwrap_or(0)),
            gib(self.peak_rss.unwrap_or(0))
        );
        let walls: Vec<f32> = self.chase.samples.iter().map(|s| s.wall_ms).collect();
        let hitches = self.hitches();
        let _ = writeln!(
            out,
            "drive: {} frames, wall p50 {:.2} ms p99 {:.2} ms max {:.2} ms; {} frames > 2x p50{}",
            walls.len(),
            percentile(&walls, 0.5),
            percentile(&walls, 0.99),
            walls.iter().copied().fold(0.0, f32::max),
            hitches.len(),
            if hitches.is_empty() {
                String::new()
            } else {
                format!(
                    " ({})",
                    hitches
                        .iter()
                        .take(12)
                        .map(|(i, w)| {
                            let s = &self.chase.samples[*i];
                            format!(
                                "#{i} {w:.1} ms [cpu {:.1}, scatter {:.1} (tiles {:.1}, contact {:.1}), gpu {:.1}]",
                                s.cpu_ms, s.scatter_ms, s.scatter_update_ms, s.contact_ms, s.gpu.frame_ms
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        );
        let t = &self.textures;
        let _ = writeln!(
            out,
            "textures: host RGBA8 {:.2} GiB in {} images; renderer CPU {:.1} MiB; GPU {:.2} GiB ({:.2} streamed, {:.2} whole) of {} budget; {} uncompressed on the GPU ({:.2} GiB), {} textures",
            gib(self.host_texture_bytes),
            self.host_uncompressed,
            mib(t.cpu_bytes),
            gib(t.resident_bytes),
            gib(t.streamed_bytes),
            gib(t.unstreamed_bytes),
            if t.budget_bytes == u64::MAX {
                "no".to_string()
            } else {
                format!("a {:.2} GiB", gib(t.budget_bytes))
            },
            t.uncompressed,
            gib(t.uncompressed_bytes),
            t.textures.len()
        );
        for e in t.textures.iter().take(5) {
            let _ = writeln!(
                out,
                "  largest: {} {} {}x{} resident {}x{} {} {:.1} MiB",
                e.name.as_deref().unwrap_or("(unnamed)"),
                e.slot,
                e.width,
                e.height,
                e.resident_width,
                e.resident_height,
                e.format,
                mib(e.gpu_bytes)
            );
        }
        for p in &self.problems {
            let _ = writeln!(out, "problem: {p}");
        }
        if self.stream.materials > 0 {
            let warmup = self.streamed_bytes.len().saturating_sub(self.chase.samples.len());
            let drive_bytes: u64 = self.streamed_bytes.iter().skip(warmup).sum();
            let _ = writeln!(
                out,
                "streaming: {} materials, {} settled, {} pending, {} fading, {} levels below cap; all settled at frame {}; {:.1} MiB streamed during the drive, max {:.1} MiB in a frame",
                self.stream.materials,
                self.stream.settled,
                self.stream.pending,
                self.stream.fading,
                self.stream.levels_below_cap,
                self.settled_frame
                    .map_or_else(|| "-".to_owned(), |f| f.to_string()),
                mib(drive_bytes),
                mib(self.streamed_bytes.iter().copied().max().unwrap_or(0))
            );
        }
        out.push_str(&self.after_drive.table(40));
        out
    }

    /// One CSV row per measured frame: `frame,wall_ms,cpu_ms,gpu_ms,scatter_ms,streamed_bytes,swapped,stream_us`.
    #[must_use]
    pub fn frames_csv(&self) -> String {
        let warmup = self.streamed_bytes.len().saturating_sub(self.chase.samples.len());
        let mut out =
            String::from("frame,wall_ms,cpu_ms,gpu_ms,scatter_ms,streamed_bytes,swapped,stream_us\n");
        for (i, s) in self.chase.samples.iter().enumerate() {
            out.push_str(&format!(
                "{i},{:.3},{:.3},{:.3},{:.3},{},{},{}\n",
                s.wall_ms,
                s.cpu_ms,
                s.gpu.frame_ms,
                s.scatter_ms,
                self.streamed_bytes.get(warmup + i).copied().unwrap_or(0),
                self.swapped.get(warmup + i).copied().unwrap_or(0),
                self.stream_us.get(warmup + i).copied().unwrap_or(0)
            ));
        }
        out
    }

    /// One CSV row per category: `map,tier,category,steady_bytes,after_drive_bytes`.
    #[must_use]
    pub fn category_csv(&self) -> String {
        let tier = if self.tier.is_empty() {
            "desktop-high"
        } else {
            &self.tier
        };
        let mut out = String::new();
        for c in MemoryCategory::ALL {
            out.push_str(&format!(
                "{},{tier},{},{},{}\n",
                self.map,
                c.name(),
                self.steady.bytes(c),
                self.after_drive.bytes(c)
            ));
        }
        out
    }
}

/// Loads `planet` from `content` and drives the chase with `options`, measuring as it goes.
/// Run one map per process: RSS and the page cache are per process / per file.
///
/// # Errors
///
/// Content loading, the GPU upload or a frame failed.
pub fn measure(
    device: &RenderDevice,
    content: &Path,
    planet: Planet,
    options: &ChaseOptions,
) -> Result<MemoryRun, LookdevError> {
    let map = crate::do27::map_name(planet);
    let map_dir = content.join("maps").join(map);
    let io_start = IoCounters::now();
    let started = Instant::now();
    let mut planets = R2Planets::default();
    planets.get(planet, 1025)?;
    let decode_s = started.elapsed().as_secs_f64();
    let mut load_s = 0.0;
    let mut io = IoCounters::default();
    let mut steady = MemoryReport::default();
    let mut after_drive = MemoryReport::default();
    let mut stream = regia_render::StreamReport::default();
    let mut textures = regia_render::TextureMemoryReport::default();
    let mut problems = Vec::new();
    let mut streamed_bytes = Vec::new();
    let mut swapped = Vec::new();
    let mut stream_us = Vec::new();
    let mut settled_frame = None;
    let last = options.warmup + options.frames - 1;
    let steady_at = options.warmup.saturating_sub(1);
    let chase = run_with(
        device,
        &mut planets,
        content,
        planet,
        options,
        &mut |i, renderer| {
            stream = renderer.stream_report();
            streamed_bytes.push(stream.uploaded_bytes);
            swapped.push(stream.swapped);
            stream_us.push(stream.pass_us);
            if settled_frame.is_none() && stream.materials > 0 && stream.settled == stream.materials {
                settled_frame = Some(i);
            }
            if i == 0 {
                let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
                load_s = started.elapsed().as_secs_f64();
                io = IoCounters::now().since(io_start);
            }
            if i == steady_at {
                steady = regia_render::memory_report(device);
            }
            if i == last {
                after_drive = regia_render::memory_report(device);
                textures = renderer.texture_memory();
                problems = renderer
                    .frame_problems()
                    .iter()
                    .map(|p| format!("{}: {}", p.code, p.message))
                    .collect();
            }
        },
    )?;
    let (host_texture_bytes, host_uncompressed) = planets
        .get(planet, 1025)
        .ok()
        .and_then(|t| t.do27.as_ref())
        .map_or((0, 0), |c| {
            let images = c.map.prop_materials.iter().flat_map(|m| {
                [&m.albedo, &m.normal_map, &m.occlusion_map, &m.emissive_map]
                    .into_iter()
                    .flatten()
            });
            images.fold((0u64, 0usize), |(b, n), t| (b + t.rgba.len() as u64, n + 1))
        });
    Ok(MemoryRun {
        map,
        tier: std::env::var("LOOK_R2_TIER").unwrap_or_default(),
        decode_s,
        load_s,
        io,
        content_bytes: dir_bytes(&map_dir),
        steady,
        after_drive,
        peak_rss: regia_render::process_peak_rss_bytes(),
        stream,
        textures,
        problems,
        host_texture_bytes,
        host_uncompressed,
        streamed_bytes,
        swapped,
        stream_us,
        settled_frame,
        chase,
    })
}

/// The runtime's headless device plus block compression (DO27's cooked terrain is
/// BC7/BC5/BC4), as `do27_chase` requests it.
///
/// # Errors
///
/// No adapter, or the device request failed.
pub fn runtime_device() -> Result<RenderDevice, LookdevError> {
    let base = RenderDevice::headless()?;
    let bc = wgpu::Features::TEXTURE_COMPRESSION_BC;
    if !base.adapter.features().contains(bc) || base.device.features().contains(bc) {
        return Ok(base);
    }
    let (device, queue) = regia_core::tasks::block_on(base.adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("mem_report device"),
        required_limits: base.device.limits(),
        required_features: base.device.features() | bc,
        ..Default::default()
    }))
    .map_err(|e| LookdevError::Config(e.to_string()))?;
    let capabilities = regia_render::RenderCapabilities::from_adapter(&base.adapter, &device);
    Ok(RenderDevice::from_parts(
        base.instance,
        base.adapter,
        device,
        queue,
        capabilities,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_counters_subtract() {
        let a = IoCounters {
            rchar: 10,
            read_bytes: 4,
        };
        let b = IoCounters {
            rchar: 25,
            read_bytes: 4,
        };
        let d = b.since(a);
        assert_eq!((d.rchar, d.read_bytes), (15, 0));
        assert_eq!(a.since(b).rchar, 0);
    }
}
