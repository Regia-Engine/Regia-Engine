// SPDX-License-Identifier: MIT-0
//! `stream_soak`: a map's worth of BC7 textures streams out of a mapped pak behind a rendered "menu"
//! frame, and every frame's wall time is recorded (`docs/specs/12-assets.md` § 5b).
//!
//! ```text
//! stream_soak --pak /nvme/soak.fpak [--make-mib 900] [--frames 900] [--no-stream] [--csv out.csv]
//! ```
//!
//! The pak (made once with `--make-mib`) holds 4096² BC7 textures with full mip chains, stored
//! uncompressed as `docs/formats/cooked-pack-guide.md` recommends. Each frame renders the hull
//! scene at 1920×1080 offscreen with a `FrameRenderer` and waits for the GPU (so the wall time
//! includes the GPU), then pulls finished assets from the `PakStreamer` (only while the
//! `UploadQueue` holds less than 64 MiB) and pumps the queue. The load set `map:soak` is
//! resident when every texture's last level is written. Prints `STREAM_SOAK {json}`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use regia_asset::{AssetId, LoadKind, LoadSet, LoadSets, PakReader, PakStreamer, StreamRequest};
use regia_render::{FrameRenderer, RenderDevice, UploadBytes, UploadQueue};

const SIZE: u32 = 4096;
const MIPS: u32 = 13;
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bc7RgbaUnormSrgb;

fn level_bytes(mip: u32) -> usize {
    let side = (SIZE >> mip).max(1).div_ceil(4) as usize;
    side * side * 16
}

fn texture_bytes() -> usize {
    (0..MIPS).map(level_bytes).sum()
}

/// Writes a v1 pak of `count` uncompressed BC7 texture blobs (pseudo-random blocks: as
/// incompressible as real BC7).
fn make_pak(path: &Path, count: usize) -> Result<(), String> {
    use std::io::Write;
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).map_err(|e| e.to_string())?);
    file.write_all(&[0u8; 60]).map_err(|e| e.to_string())?;
    let mut offset = 60u64;
    let mut entries = Vec::new();
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let blob = texture_bytes();
    let mut bytes = vec![0u8; blob];
    for _ in 0..count {
        let pad = offset.next_multiple_of(regia_asset::pak::FPAK_BLOB_ALIGNMENT) - offset;
        file.write_all(&vec![0u8; pad as usize])
            .map_err(|e| e.to_string())?;
        offset += pad;
        for chunk in bytes.chunks_mut(8) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
        }
        file.write_all(&bytes).map_err(|e| e.to_string())?;
        entries.push(regia_asset::pak::PakEntry {
            id: AssetId::new_random(),
            type_tag: regia_core::id::TypePathHash(0),
            offset,
            size: blob as u64,
            compressed_size: blob as u64,
            block_table_offset: 0,
        });
        offset += blob as u64;
    }
    let toc = regia_asset::pak::PakToc {
        entries,
        manifest: Vec::new(),
        dependencies: Vec::new(),
    };
    let toc_bytes = postcard::to_allocvec(&toc).map_err(|e| e.to_string())?;
    file.write_all(&toc_bytes).map_err(|e| e.to_string())?;
    let mut file = file.into_inner().map_err(|e| e.to_string())?;
    let header = regia_asset::encode_pak_header(
        regia_asset::pak::FPAK_VERSION,
        0,
        offset,
        toc_bytes.len() as u64,
        &[0; 32],
    );
    use std::io::Seek;
    file.seek(std::io::SeekFrom::Start(0))
        .map_err(|e| e.to_string())?;
    file.write_all(&header).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())
}

/// The headless device plus BC compression (as `lookdev::mem_budget::runtime_device`).
fn device() -> Result<RenderDevice, String> {
    let base = RenderDevice::headless().map_err(|e| e.to_string())?;
    let bc = wgpu::Features::TEXTURE_COMPRESSION_BC;
    if !base.adapter.features().contains(bc) {
        return Err("adapter has no BC compression".into());
    }
    if base.device.features().contains(bc) {
        return Ok(base);
    }
    let (device, queue) = regia_core::tasks::block_on(base.adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("stream_soak device"),
        required_limits: base.device.limits(),
        required_features: base.device.features() | bc,
        ..Default::default()
    }))
    .map_err(|e| e.to_string())?;
    let capabilities = regia_render::RenderCapabilities::from_adapter(&base.adapter, &device);
    Ok(RenderDevice::from_parts(
        base.instance,
        base.adapter,
        device,
        queue,
        capabilities,
    ))
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn percentile(sorted: &[f32], p: f32) -> f32 {
    regia_render::hitch::percentile_sorted(sorted, p)
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("stream_soak: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[allow(clippy::too_many_lines)] // one linear benchmark
fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let pak_path = PathBuf::from(arg(&args, "--pak").ok_or("--pak PATH is required")?);
    let frames: usize = arg(&args, "--frames").and_then(|v| v.parse().ok()).unwrap_or(900);
    let stream = !args.iter().any(|a| a == "--no-stream");
    if let Some(mib) = arg(&args, "--make-mib").and_then(|v| v.parse::<usize>().ok())
        && !pak_path.exists()
    {
        let count = (mib * 1024 * 1024).div_ceil(texture_bytes());
        let t = Instant::now();
        make_pak(&pak_path, count)?;
        eprintln!(
            "made {} ({count} textures) in {:.1} s",
            pak_path.display(),
            t.elapsed().as_secs_f32()
        );
    }
    let device = device()?;
    let open = Instant::now();
    let reader = Arc::new(PakReader::open(&pak_path).map_err(|e| e.to_string())?);
    let open_ms = open.elapsed().as_secs_f32() * 1000.0;
    let ids: Vec<AssetId> = reader.toc.entries.iter().map(|e| e.id).collect();
    let sets = LoadSets::default();
    let mut set = LoadSet::new("map:soak");
    for (i, _) in ids.iter().enumerate() {
        set.push(LoadKind::Texture, format!("tex{i}"), 0, texture_bytes() as u64);
    }
    sets.define(set);

    let scene = regia_render::hard_surface::hull_closeup_scene();
    let mut frame_scene = scene.as_frame_scene();
    frame_scene.width = 1920;
    frame_scene.height = 1080;
    let mut renderer = FrameRenderer::new();
    // Warm-up: pipelines, targets.
    for _ in 0..30 {
        renderer
            .begin_offscreen(&device, &frame_scene, wgpu::TextureFormat::Rgba8UnormSrgb)
            .map_err(|e| e.to_string())?;
        device.wait_idle().map_err(|e| e.to_string())?;
    }

    let streamer = PakStreamer::new(vec![reader.clone()], Some(sets.clone())).map_err(|e| e.to_string())?;
    let mut uploads = UploadQueue::default();
    let mut textures: Vec<wgpu::Texture> = Vec::new();
    let start = Instant::now();
    if stream {
        streamer.request(ids.iter().enumerate().map(|(i, id)| StreamRequest {
            id: *id,
            kind: LoadKind::Texture,
            key: format!("tex{i}"),
        }));
    }
    let mut wall = Vec::with_capacity(frames);
    let mut rows = String::from("frame,wall_ms,stream_ms,pump_ms,upload_bytes,pending_mib\n");
    // Main-thread streaming cost per frame (texture creation + band writes) and the writes alone.
    let mut stream_cost = Vec::with_capacity(frames);
    let mut pump_cost = Vec::with_capacity(frames);
    let mut resident_at: Option<(usize, f32)> = None;
    // Anonymous (heap) RSS peak: `VmHWM` also counts the mapped pak's file pages, which a
    // streamed map touches either way; heap copies of asset bytes show up only here.
    let mut peak_anon_kib = proc_status_kib("RssAnon:").unwrap_or(0);
    // The same § 2.4 verdict `play.hitch_report` gives (warm-up already excluded above).
    let mut hitch = regia_render::HitchRecorder::new();
    hitch.frame(0.0, 0.0, 0.0);
    for frame in 0..frames {
        let t0 = Instant::now();
        renderer
            .begin_offscreen(&device, &frame_scene, wgpu::TextureFormat::Rgba8UnormSrgb)
            .map_err(|e| e.to_string())?;
        let s0 = Instant::now();
        // Take finished assets only while the queue is short (bounded RAM).
        while uploads.stats().pending_bytes < 64 * 1024 * 1024 {
            let Some(asset) = streamer.try_next() else {
                break;
            };
            let texture = device.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("material texture streamed"),
                size: wgpu::Extent3d {
                    width: SIZE,
                    height: SIZE,
                    depth_or_array_layers: 1,
                },
                mip_level_count: MIPS,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let data = Arc::new(asset.bytes);
            let mut at = 0usize;
            // Coarse levels first: the texture is usable at low detail early.
            let ranges: Vec<(u32, std::ops::Range<usize>)> = (0..MIPS)
                .map(|m| {
                    let r = at..at + level_bytes(m);
                    at = r.end;
                    (m, r)
                })
                .collect();
            for (mip, range) in ranges.into_iter().rev() {
                let done = (mip == 0).then(|| {
                    let sets = sets.clone();
                    let key = asset.request.key.clone();
                    Box::new(move || sets.mark_resident(LoadKind::Texture, &key))
                        as regia_render::upload_queue::UploadDone
                });
                uploads.push_texture_level(
                    asset.request.key.clone(),
                    &texture,
                    mip,
                    0,
                    UploadBytes::slice(data.clone(), range),
                    done,
                );
            }
            textures.push(texture);
        }
        let pumped = uploads.pump(&device.queue);
        let stream_ms = s0.elapsed().as_secs_f32() * 1000.0;
        // The harness waits for the GPU to time whole frames; it waits on the raw device so the
        // hitch verdict counts only the engine's own blocking polls, not this measurement sync.
        device
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| e.to_string())?;
        let ms = t0.elapsed().as_secs_f32() * 1000.0;
        wall.push(ms);
        hitch.frame(ms, 0.0, 0.0);
        peak_anon_kib = peak_anon_kib.max(proc_status_kib("RssAnon:").unwrap_or(0));
        let pump_ms = pumped.last_frame_us / 1000.0;
        stream_cost.push(stream_ms);
        pump_cost.push(pump_ms);
        rows.push_str(&format!(
            "{frame},{ms:.3},{stream_ms:.3},{pump_ms:.3},{},{:.1}\n",
            pumped.last_frame_bytes,
            pumped.pending_bytes as f32 / 1048576.0
        ));
        if stream && resident_at.is_none() && sets.is_resident("map:soak") {
            resident_at = Some((frame, start.elapsed().as_secs_f32()));
        }
    }
    let mut sorted = wall.clone();
    sorted.sort_by(f32::total_cmp);
    let p50 = percentile(&sorted, 0.5);
    let over = wall.iter().filter(|&&w| w > 2.0 * p50).count();
    let progress = streamer.progress();
    let hitch = hitch.report(0, regia_render::HitchTargets::SPEC);
    stream_cost.sort_by(f32::total_cmp);
    pump_cost.sort_by(f32::total_cmp);
    let report = serde_json::json!({
        "stream": stream,
        "adapter": device.adapter.get_info().name,
        "pak_mib": reader.len_bytes() as f64 / 1048576.0,
        "pak_mapped": reader.is_mapped(),
        "open_ms": open_ms,
        "textures": ids.len(),
        "frames": frames,
        "p50_ms": p50,
        "p99_ms": percentile(&sorted, 0.99),
        "max_ms": sorted.last().copied().unwrap_or(0.0),
        "frames_over_2x_p50": over,
        "stream_cost_p99_ms": percentile(&stream_cost, 0.99),
        "stream_cost_max_ms": stream_cost.last().copied().unwrap_or(0.0),
        "pump_p99_ms": percentile(&pump_cost, 0.99),
        "pump_max_ms": pump_cost.last().copied().unwrap_or(0.0),
        "hitch_passes": hitch.passes(),
        "hitch_failures": hitch.failures(),
        "hitch_p99_over_p50": hitch.p99_ms / hitch.p50_ms.max(1e-3),
        "frames_with_creates": hitch.frames_with_creates,
        "streamed_textures_created": textures.len(),
        "resident_frame": resident_at.map(|r| r.0),
        "resident_s": resident_at.map(|r| r.1),
        "streamed_mib": progress.bytes as f64 / 1048576.0,
        "read_busy_ms": progress.busy_us as f64 / 1000.0,
        "upload_rate_mb_s": uploads.stats().rate_bytes_per_us,
        "max_write_kib": uploads.stats().max_call_bytes / 1024,
        "rss_mib": regia_render::process_rss_bytes().map(|b| b / 1048576),
        "peak_rss_mib": regia_render::process_peak_rss_bytes().map(|b| b / 1048576),
        "peak_anon_mib": peak_anon_kib / 1024,
        "rss_anon_mib": proc_status_kib("RssAnon:").map(|k| k / 1024),
        "rss_file_mib": proc_status_kib("RssFile:").map(|k| k / 1024),
        "vram_mib": regia_render::memory_report(&device).allocated_bytes / 1048576,
    });
    if let Some(csv) = arg(&args, "--csv") {
        std::fs::write(&csv, rows).map_err(|e| format!("{csv}: {e}"))?;
    }
    println!("STREAM_SOAK {report}");
    drop(streamer);
    std::thread::sleep(Duration::from_millis(10));
    Ok(())
}

/// A `kB` field of `/proc/self/status` (Linux; `None` elsewhere).
fn proc_status_kib(key: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix(key))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
}
