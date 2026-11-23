// SPDX-License-Identifier: MPL-2.0
//! XeSS Super Resolution on a real hardware GPU (ADR-4656).
//!
//! Skips unless `REGIA_XESS_SMOKE=1`; then it fails when XeSS cannot run. Run on a host with an
//! hardware GPU and the XeSS runtime in `REGIA_XESS_PATH` (an absolute directory):
//!
//! ```text
//! XESS_SDK=/abs/XeSS REGIA_XESS_SMOKE=1 REGIA_XESS_PATH=/abs/XeSS/bin \
//!   cargo test -p regia_xess --features sdk --test gpu_smoke -- --nocapture
//! ```
//!
//! A static analytic scene (a hard-edged disk and a rotated checkerboard, built on the CPU) is
//! rendered at the render size with Halton jitter, each sample taken where the projection's
//! offset puts it, and handed to XeSS for 48 frames. The output is compared with the scene
//! supersampled at the display size, and with a bilinear upscale of one render frame. XeSS must
//! beat bilinear clearly, and passing the jitter as the projection's offset in render pixels,
//! y down (the convention `regia_render`'s seam uses), must beat the flipped sign: this is the
//! sign check ADR-1150 left open.

use regia_xess::{CreateDesc, EvalInputs, Quality, Runtime};

const DISPLAY: [u32; 2] = [960, 540];
const FRAMES: u64 = 48;
/// Diagnostic run counter for trace lines.
static RUN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The scene at display-normalised `(u, v)`, y down: a disk and a rotated checkerboard.
fn scene(u: f32, v: f32) -> f32 {
    let (du, dv) = (u - 0.3, (v - 0.5) * DISPLAY[1] as f32 / DISPLAY[0] as f32);
    if du * du + dv * dv < 0.18 * 0.18 {
        return 0.9;
    }
    let (x, y) = (u * DISPLAY[0] as f32, v * DISPLAY[1] as f32);
    let angle = 0.35f32;
    let (s, c) = angle.sin_cos();
    let (rx, ry) = (x * c + y * s, -x * s + y * c);
    let cell = 7.0;
    if ((rx / cell).floor() as i64 + (ry / cell).floor() as i64).rem_euclid(2) == 0 {
        0.75
    } else {
        0.1
    }
}

fn truth() -> Vec<f32> {
    let n = 4;
    let mut out = Vec::with_capacity((DISPLAY[0] * DISPLAY[1]) as usize);
    for y in 0..DISPLAY[1] {
        for x in 0..DISPLAY[0] {
            let mut sum = 0.0;
            for j in 0..n {
                for i in 0..n {
                    let u = (x as f32 + (i as f32 + 0.5) / n as f32) / DISPLAY[0] as f32;
                    let v = (y as f32 + (j as f32 + 0.5) / n as f32) / DISPLAY[1] as f32;
                    sum += scene(u, v);
                }
            }
            out.push(sum / (n * n) as f32);
        }
    }
    out
}

fn halton(mut index: u32, base: u32) -> f32 {
    let (mut f, mut r) = (1.0f32, 0.0f32);
    while index > 0 {
        f /= base as f32;
        r += f * (index % base) as f32;
        index /= base;
    }
    r
}

/// The projection's offset of `frame` in render pixels, y down (the seam's `render_pixels`).
fn jitter(frame: u64, phases: u64) -> [f32; 2] {
    let n = (frame % phases) as u32 + 1;
    [halton(n, 2) - 0.5, -(halton(n, 3) - 0.5)]
}

/// One render frame: each pixel samples the scene where a projection moved by `offset` (pixels,
/// y down) puts its centre, i.e. at centre − offset.
fn render(size: [u32; 2], offset: [f32; 2]) -> Vec<f32> {
    let mut out = Vec::with_capacity((size[0] * size[1]) as usize);
    for y in 0..size[1] {
        for x in 0..size[0] {
            let u = (x as f32 + 0.5 - offset[0]) / size[0] as f32;
            let v = (y as f32 + 0.5 - offset[1]) / size[1] as f32;
            out.push(scene(u, v));
        }
    }
    out
}

fn bilinear(image: &[f32], size: [u32; 2]) -> Vec<f32> {
    let at = |x: i64, y: i64| {
        let x = x.clamp(0, i64::from(size[0]) - 1) as usize;
        let y = y.clamp(0, i64::from(size[1]) - 1) as usize;
        image[y * size[0] as usize + x]
    };
    let mut out = Vec::with_capacity((DISPLAY[0] * DISPLAY[1]) as usize);
    for y in 0..DISPLAY[1] {
        for x in 0..DISPLAY[0] {
            let fx = (x as f32 + 0.5) * size[0] as f32 / DISPLAY[0] as f32 - 0.5;
            let fy = (y as f32 + 0.5) * size[1] as f32 / DISPLAY[1] as f32 - 0.5;
            let (x0, y0) = (fx.floor(), fy.floor());
            let (tx, ty) = (fx - x0, fy - y0);
            let (x0, y0) = (x0 as i64, y0 as i64);
            let top = at(x0, y0) * (1.0 - tx) + at(x0 + 1, y0) * tx;
            let bottom = at(x0, y0 + 1) * (1.0 - tx) + at(x0 + 1, y0 + 1) * tx;
            out.push(top * (1.0 - ty) + bottom * ty);
        }
    }
    out
}

fn rmse(a: &[f32], b: &[f32]) -> f32 {
    let sum: f64 = a.iter().zip(b).map(|(x, y)| f64::from(x - y).powi(2)).sum();
    (sum / a.len() as f64).sqrt() as f32
}

/// Diagnostic: how many display pixels differ by more than 1e-3, and their bounding box.
fn difference_summary(a: &[f32], b: &[f32]) -> String {
    let (mut count, mut max) = (0usize, 0.0f32);
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for (index, (x, y)) in a.iter().zip(b).enumerate() {
        let delta = (x - y).abs();
        if delta > 1e-3 {
            let (px, py) = (index as u32 % DISPLAY[0], index as u32 / DISPLAY[0]);
            count += 1;
            max = max.max(delta);
            (x0, y0, x1, y1) = (x0.min(px), y0.min(py), x1.max(px), y1.max(py));
        }
    }
    format!("{count} pixels over 1e-3, max {max}, box ({x0},{y0})-({x1},{y1})")
}

fn f16_to_f32(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = i32::from((bits >> 10) & 0x1f);
    let frac = f32::from(bits & 0x3ff);
    match exp {
        0 => sign * frac * 2f32.powi(-24),
        31 => f32::NAN,
        _ => sign * (1.0 + frac / 1024.0) * 2f32.powi(exp - 15),
    }
}

fn f32_to_f16(value: f32) -> u16 {
    // Values here are in [0, 1]: normal halves only, round to nearest.
    if value <= 0.0 {
        return 0;
    }
    let bits = value.to_bits();
    let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    if exp <= 0 {
        return 0;
    }
    let mant = (bits >> 13) & 0x3ff;
    let round = (bits >> 12) & 1;
    (((exp as u32) << 10) | mant) as u16 + round as u16
}

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    runtime: Runtime,
}

fn open() -> Gpu {
    let path = std::env::var_os("REGIA_XESS_PATH")
        .map(std::path::PathBuf::from)
        .expect("REGIA_XESS_PATH must name the external SDK bin directory")
        .join("libxess.dll");
    let runtime = Runtime::load(&path).expect("load XeSS runtime");
    let descriptor = wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    };
    let (instance, instance_ext) = runtime.create_instance(&descriptor).expect("XeSS instance");
    let adapters = regia_core::tasks::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN));
    let requested = std::env::var("REGIA_XESS_GPU").ok();
    let adapter = adapters
        .into_iter()
        .find(|a| {
            let info = a.get_info();
            info.device_type != wgpu::DeviceType::Cpu
                && requested
                    .as_ref()
                    .is_none_or(|name| info.name.contains(name.as_str()))
        })
        .expect("hardware Vulkan adapter matching REGIA_XESS_GPU, when set");
    let info = adapter.get_info();
    let descriptor = wgpu::DeviceDescriptor {
        label: Some("xess smoke"),
        ..Default::default()
    };
    let (device, queue, device_ext) = runtime
        .request_device(&adapter, &descriptor)
        .expect("XeSS device");
    eprintln!(
        "adapter {} driver {} {}; SDK {}; SR {:?}; instance extensions {instance_ext:?}; device extensions {device_ext:?}",
        info.name,
        info.driver,
        info.driver_info,
        regia_xess::SDK_VERSION,
        runtime.version()
    );
    assert!(regia_xess::device_prepared(&device));
    Gpu {
        device,
        queue,
        runtime,
    }
}

fn texture(
    gpu: &Gpu,
    label: &str,
    size: [u32; 2],
    format: wgpu::TextureFormat,
    usage: wgpu::TextureUsages,
) -> wgpu::Texture {
    gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: size[0],
            height: size[1],
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    })
}

fn upload(gpu: &Gpu, texture: &wgpu::Texture, bytes: &[u8], bytes_per_pixel: u32) {
    let size = texture.size();
    gpu.queue.write_texture(
        texture.as_image_copy(),
        bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(size.width * bytes_per_pixel),
            rows_per_image: Some(size.height),
        },
        size,
    );
}

fn read_luma(gpu: &Gpu, output: &wgpu::Texture) -> Vec<f32> {
    let row = (DISPLAY[0] * 8).div_ceil(256) * 256;
    let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("xess readback"),
        size: u64::from(row * DISPLAY[1]),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.copy_texture_to_buffer(
        output.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(DISPLAY[1]),
            },
        },
        output.size(),
    );
    gpu.queue.submit([encoder.finish()]);
    buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let data = buffer.slice(..).get_mapped_range().expect("mapped readback");
    let mut out = Vec::with_capacity((DISPLAY[0] * DISPLAY[1]) as usize);
    for y in 0..DISPLAY[1] as usize {
        for x in 0..DISPLAY[0] as usize {
            let at = y * row as usize + x * 8;
            let r = f16_to_f32(u16::from_le_bytes([data[at], data[at + 1]]));
            out.push(r);
        }
    }
    out
}

/// Runs the sequence through XeSS with the jitter passed as `sign` × the projection's offset and
/// returns the last output's red channel.
fn run(gpu: &Gpu, quality: Quality, sign: f32, motion_format: wgpu::TextureFormat) -> (Vec<f32>, [u32; 2]) {
    let mut sr = gpu.runtime.context(&gpu.device).expect("create XeSS context");
    let settings = sr.optimal_settings(DISPLAY, quality).expect("optimal settings");
    let size = settings.render;
    sr.initialize(CreateDesc {
        display: DISPLAY,
        quality,
        hdr: true,
        depth_inverted: false,
        motion_jittered: false,
    })
    .expect("initialize XeSS");
    // Repeatable output needs app-owned zeroed temporary heaps (ADR-4907).
    assert!(
        sr.zeroed_temp_heaps(),
        "XeSS did not take the zeroed temporary heaps"
    );
    eprintln!(
        "{quality:?} temporary heaps zeroed by {:?} (buffer, texture)",
        sr.temp_heap_zeroing()
            .map(|methods| methods.map(regia_xess::HeapZeroing::code))
    );
    let sampled = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST;
    let color = texture(gpu, "color", size, wgpu::TextureFormat::Rgba16Float, sampled);
    let motion = texture(gpu, "motion", size, motion_format, sampled);
    let depth = texture(
        gpu,
        "depth",
        size,
        wgpu::TextureFormat::Depth32Float,
        wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
    );
    let output = texture(
        gpu,
        "output",
        DISPLAY,
        wgpu::TextureFormat::Rgba16Float,
        wgpu::TextureUsages::STORAGE_BINDING
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
    );
    let motion_bytes = if motion_format == wgpu::TextureFormat::Rg16Float {
        4
    } else {
        8
    };
    upload(
        gpu,
        &motion,
        &vec![0u8; (size[0] * size[1] * motion_bytes) as usize],
        motion_bytes,
    );
    // wgpu zero-fills a texture it never saw written before its first read, and XeSS's writes
    // are invisible to it: write the output once through wgpu (EvalInputs::output).
    upload(
        gpu,
        &output,
        &vec![0u8; (DISPLAY[0] * DISPLAY[1] * 8) as usize],
        8,
    );
    let ratio = DISPLAY[0] as f32 / size[0] as f32;
    let phases = ((8.0 * ratio * ratio).round() as u64).max(8);
    // Diagnostic view lifetimes (default `fresh`: new views every frame, dropped after recording).
    // `retain` keeps every frame's views until the run ends; `persistent` reuses one set.
    let view_mode = std::env::var("REGIA_XESS_SMOKE_VIEWS").unwrap_or_else(|_| "fresh".into());
    let make_views =
        || [&color, &motion, &depth, &output].map(|t| t.create_view(&wgpu::TextureViewDescriptor::default()));
    let persistent = make_views();
    let mut retained = Vec::new();
    let limit = std::env::var("REGIA_XESS_SMOKE_INFLIGHT")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|&limit| limit > 0);
    let mut in_flight = std::collections::VecDeque::new();
    // Optional diagnostic: repeat the reset sequence in this same context and compare outputs.
    let sequences = std::env::var("REGIA_XESS_SMOKE_SEQUENCES")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1)
        .max(1);
    let mut sequence_images = Vec::new();
    let trace = std::env::var("REGIA_XESS_SMOKE_TRACE").as_deref() == Ok("1");
    let run_id = RUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let start = std::time::Instant::now();
    for step in 0..FRAMES * sequences {
        let frame = step % FRAMES;
        let views = if view_mode == "persistent" {
            persistent.clone()
        } else {
            make_views()
        };
        let offset = jitter(frame, phases);
        let image = render(size, offset);
        let mut bytes = Vec::with_capacity(image.len() * 8);
        for value in image {
            let h = f32_to_f16(value).to_le_bytes();
            let one = f32_to_f16(1.0).to_le_bytes();
            bytes.extend_from_slice(&[h[0], h[1], h[0], h[1], h[0], h[1], one[0], one[1]]);
        }
        upload(gpu, &color, &bytes, 8);
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let _clear = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("depth clear"),
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &views[2],
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(0.5),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                ..Default::default()
            });
        }
        let inputs = EvalInputs {
            color: &views[0],
            depth: &views[2],
            motion: &views[1],
            exposure: 1.0,
            output: &views[3],
            jitter: [offset[0] * sign, offset[1] * sign],
            motion_scale: [size[0] as f32, size[1] as f32],
            render_size: size,
            reset: frame == 0,
        };
        let xess = sr.evaluate(&gpu.device, &mut encoder, &inputs).expect("evaluate");
        // The recorded command buffer must retain every view after caller handles are dropped.
        if view_mode == "retain" {
            retained.push(views);
        } else {
            drop(views);
        }
        in_flight.push_back(xess.submit(&gpu.queue, encoder.finish()));
        // Optional diagnostic: distinguish queued-frame behavior from context reuse without
        // changing the normal smoke's submission pattern or any image-quality assertion.
        if std::env::var("REGIA_XESS_SMOKE_SERIAL").as_deref() == Ok("1") {
            gpu.device
                .poll(wgpu::PollType::wait_indefinitely())
                .expect("serial diagnostic poll");
        }
        // Optional diagnostic: at most `limit` submitted frames unfinished (1 equals serial).
        while limit.is_some_and(|limit| in_flight.len() >= limit) {
            let oldest = in_flight.pop_front();
            gpu.device
                .poll(wgpu::PollType::Wait {
                    submission_index: oldest,
                    timeout: None,
                })
                .expect("in-flight diagnostic poll");
        }
        if sequences > 1 && frame == FRAMES - 1 {
            sequence_images.push(read_luma(gpu, &output));
        }
        // Optional diagnostic: a hash of every frame's output (waits for each frame).
        if trace {
            let hash = read_luma(gpu, &output)
                .iter()
                .fold(0xcbf2_9ce4_8422_2325u64, |h, v| {
                    (h ^ u64::from(v.to_bits())).wrapping_mul(0x100_0000_01b3)
                });
            eprintln!(
                "trace {quality:?} run {run_id} sign {sign} motion {motion_format:?} step {step} hash {hash:016x}"
            );
        }
    }
    for (index, image) in sequence_images.iter().enumerate().skip(1) {
        eprintln!(
            "{quality:?} same-context sequence {index} vs 0: RMSE {}, {}",
            rmse(image, &sequence_images[0]),
            difference_summary(image, &sequence_images[0])
        );
    }
    let image = read_luma(gpu, &output);
    drop(retained);
    eprintln!(
        "{quality:?} views {view_mode}, in-flight limit {limit:?}; submit/upload/complete wall ms per frame: {:.3}",
        start.elapsed().as_secs_f64() * 1000.0 / FRAMES as f64
    );
    (image, size)
}

#[test]
fn xess_upscales_a_jittered_sequence_close_to_the_native_frame() {
    if std::env::var("REGIA_XESS_SMOKE").as_deref() != Ok("1") {
        eprintln!("skipped: set REGIA_XESS_SMOKE=1 on a host with a hardware GPU and the XeSS runtime");
        return;
    }
    let gpu = open();
    let truth = truth();
    // Optional diagnostic: `REGIA_XESS_SMOKE_ORDER=performance` runs Performance first.
    let mut order = [Quality::Quality, Quality::Performance];
    if std::env::var("REGIA_XESS_SMOKE_ORDER").as_deref() == Ok("performance") {
        order.reverse();
    }
    for quality in order {
        // Keep each mode's cold quality covered, then compare equally warm SDK contexts.
        let (cold, cold_size) = run(&gpu, quality, 1.0, wgpu::TextureFormat::Rg16Float);
        let cold_base = bilinear(&render(cold_size, [0.0, 0.0]), cold_size);
        assert!(
            rmse(&cold, &truth) < 0.8 * rmse(&cold_base, &truth),
            "cold XeSS quality regressed"
        );
        let (projection, size) = run(&gpu, quality, 1.0, wgpu::TextureFormat::Rg16Float);
        let (flipped, _) = run(&gpu, quality, -1.0, wgpu::TextureFormat::Rg16Float);
        let (engine_motion, _) = run(&gpu, quality, 1.0, wgpu::TextureFormat::Rgba16Float);
        let (repeat, _) = run(&gpu, quality, 1.0, wgpu::TextureFormat::Rg16Float);
        eprintln!(
            "{quality:?} differences: repeat {}; converted {}",
            difference_summary(&repeat, &projection),
            difference_summary(&engine_motion, &projection)
        );
        eprintln!(
            "{quality:?} control: repeat RG16 RMSE {}, original/native {}, converted/native {}",
            rmse(&repeat, &projection),
            rmse(&projection, &truth),
            rmse(&engine_motion, &truth)
        );
        let conversion_error = rmse(&engine_motion, &projection);
        eprintln!("{quality:?} RGBA16 motion conversion RMSE vs RG16: {conversion_error:.6}");
        assert!(
            conversion_error < 0.0001,
            "motion conversion changed the XeSS result"
        );
        let base = bilinear(&render(size, [0.0, 0.0]), size);
        let (e_proj, e_flip, e_bilinear) = (
            rmse(&projection, &truth),
            rmse(&flipped, &truth),
            rmse(&base, &truth),
        );
        eprintln!(
            "{quality:?} render {size:?} -> {DISPLAY:?}: RMSE vs native  XeSS (projection sign) {e_proj:.4}  \
             XeSS (flipped sign) {e_flip:.4}  bilinear {e_bilinear:.4}"
        );
        assert!(
            rmse(&engine_motion, &truth) < 0.8 * e_bilinear,
            "converted motion quality regressed"
        );
        assert!(
            rmse(&repeat, &projection) < 0.0001,
            "equally warm RG16 sequences differ"
        );
        assert!(
            e_proj.is_finite() && e_proj < 0.8 * e_bilinear,
            "XeSS is not clearly better than bilinear"
        );
        assert!(
            e_proj < e_flip,
            "the flipped jitter sign upscaled better: the seam's sign is wrong"
        );
    }
}
