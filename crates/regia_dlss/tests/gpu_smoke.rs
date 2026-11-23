// SPDX-License-Identifier: MPL-2.0
//! DLSS Super Resolution on a real NVIDIA GPU (ADR-2002).
//!
//! Skips unless `REGIA_DLSS_SMOKE=1`; then it fails when DLSS cannot run. Run on a host with an
//! NVIDIA GPU and the DLSS runtime in `REGIA_DLSS_PATH` (an absolute directory):
//!
//! ```text
//! DLSS_SDK=/abs/DLSS REGIA_DLSS_SMOKE=1 REGIA_DLSS_PATH=/abs/DLSS/lib/Linux_x86_64/rel \
//!   REGIA_DLSS_SMOKE_PCI=0000:01:00.0 cargo test -p regia_dlss --features sdk --test gpu_smoke -- --nocapture
//! ```
//!
//! A static analytic scene (a hard-edged disk and a rotated checkerboard, built on the CPU) is
//! rendered at the render size with Halton jitter, each sample taken where the projection's
//! offset puts it, and handed to DLSS for 48 frames. The output is compared with the scene
//! supersampled at the display size, and with a bilinear upscale of one render frame. DLSS must
//! beat bilinear clearly, and passing the jitter as the projection's offset in render pixels,
//! y down (the convention `regia_render`'s seam uses), must beat the flipped sign: this is the
//! sign check ADR-1150 left open.

use regia_dlss::{CreateFlags, EvalInputs, Ngx, NgxConfig, PerfQuality, SuperResolution};

const DISPLAY: [u32; 2] = [960, 540];
const FRAMES: u64 = 48;

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
    ngx: Ngx,
}

fn open() -> Gpu {
    let (device, queue, config) = open_device();
    let ngx = match Ngx::new(&config, &device) {
        Ok(ngx) => ngx,
        Err(error) => panic!("Ngx::new: {} ({error})", error.code()),
    };
    Gpu { device, queue, ngx }
}

/// Ends a value's life at a chosen point. `std::mem::drop` is flagged on the check-stub build, where
/// these NGX types carry no drop glue; the explicit call still marks where the release belongs on
/// the linked build, which is what the lifecycle tests below order.
fn release<T>(_value: T) {}

fn open_device() -> (wgpu::Device, wgpu::Queue, NgxConfig) {
    let dirs: Vec<std::path::PathBuf> = std::env::var_os("REGIA_DLSS_PATH")
        .map(Into::into)
        .into_iter()
        .collect();
    let config = NgxConfig::new(dirs);
    let descriptor = wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    };
    let (instance, instance_ext) = match regia_dlss::create_instance(&descriptor, &config) {
        Ok(created) => created,
        Err(error) => panic!("create_instance: {} ({error})", error.code()),
    };
    let wanted_pci = std::env::var("REGIA_DLSS_SMOKE_PCI").ok();
    let adapters = regia_core::tasks::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN));
    let adapter = adapters
        .into_iter()
        .filter(|a| a.get_info().vendor == 0x10DE)
        .find(|a| {
            wanted_pci
                .as_deref()
                .is_none_or(|pci| a.get_info().device_pci_bus_id.eq_ignore_ascii_case(pci))
        });
    let Some(adapter) = adapter else {
        panic!("no NVIDIA Vulkan adapter (REGIA_DLSS_SMOKE_PCI={wanted_pci:?})");
    };
    let info = adapter.get_info();
    let descriptor = wgpu::DeviceDescriptor {
        label: Some("dlss smoke"),
        required_limits: wgpu::Limits::default(),
        ..Default::default()
    };
    let (device, queue, device_ext) = match regia_dlss::request_device(&adapter, &descriptor, &config) {
        Ok(created) => created,
        Err(error) => panic!("request_device: {} ({error})", error.code()),
    };
    eprintln!(
        "adapter {} {} driver {}; SDK {}; instance extensions {instance_ext:?}; device extensions {device_ext:?}",
        info.name,
        info.device_pci_bus_id,
        info.driver_info,
        regia_dlss::SDK_VERSION
    );
    assert!(
        !device_ext.is_empty(),
        "NGX asked for no device extensions or the adapter lacks them"
    );
    (device, queue, config)
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
        label: Some("dlss readback"),
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

/// Runs the sequence through DLSS with the jitter passed as `sign` × the projection's offset and
/// returns the last output's red channel.
fn run(gpu: &Gpu, quality: PerfQuality, sign: f32) -> (Vec<f32>, [u32; 2]) {
    let settings = gpu
        .ngx
        .optimal_settings(DISPLAY, quality)
        .expect("optimal settings");
    let size = settings.render;
    let flags = CreateFlags {
        hdr: true,
        depth_inverted: true,
        motion_low_res: true,
        motion_jittered: false,
        auto_exposure: false,
    };
    let mut sr = SuperResolution::new(
        &gpu.ngx,
        &gpu.device,
        &gpu.queue,
        DISPLAY,
        quality,
        flags,
        settings.max,
    )
    .expect("create the DLSS feature");
    let sampled = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST;
    let color = texture(gpu, "color", size, wgpu::TextureFormat::Rgba16Float, sampled);
    let motion = texture(gpu, "motion", size, wgpu::TextureFormat::Rg16Float, sampled);
    let exposure = texture(gpu, "exposure", [1, 1], wgpu::TextureFormat::R32Float, sampled);
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
    upload(gpu, &motion, &vec![0u8; (size[0] * size[1] * 4) as usize], 4);
    upload(gpu, &exposure, &1.0f32.to_le_bytes(), 4);
    // wgpu zero-fills a texture it never saw written before its first read, and NGX's writes
    // are invisible to it: write the output once through wgpu (EvalInputs::output).
    upload(
        gpu,
        &output,
        &vec![0u8; (DISPLAY[0] * DISPLAY[1] * 8) as usize],
        8,
    );
    let views = [&color, &motion, &exposure, &depth, &output]
        .map(|t| t.create_view(&wgpu::TextureViewDescriptor::default()));
    let ratio = DISPLAY[0] as f32 / size[0] as f32;
    let phases = ((8.0 * ratio * ratio).round() as u64).max(8);
    for frame in 0..FRAMES {
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
                    view: &views[3],
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
            depth: &views[3],
            motion: &views[1],
            exposure: Some(&views[2]),
            reactive: None,
            transparency: None,
            output: &views[4],
            jitter: [offset[0] * sign, offset[1] * sign],
            motion_scale: [size[0] as f32, size[1] as f32],
            render_size: size,
            reset: frame == 0,
            frame_time_ms: 16.6,
        };
        let dlss = sr.evaluate(&gpu.device, &mut encoder, &inputs).expect("evaluate");
        dlss.submit(&gpu.queue, encoder.finish());
    }
    (read_luma(gpu, &output), size)
}

#[test]
fn dlss_upscales_a_jittered_sequence_close_to_the_native_frame() {
    if std::env::var("REGIA_DLSS_SMOKE").as_deref() != Ok("1") {
        eprintln!("skipped: set REGIA_DLSS_SMOKE=1 on a host with an NVIDIA GPU and the DLSS runtime");
        return;
    }
    let gpu = open();
    let truth = truth();
    for quality in [
        PerfQuality::Dlaa,
        PerfQuality::Quality,
        PerfQuality::Balanced,
        PerfQuality::Performance,
        PerfQuality::UltraPerformance,
    ] {
        let (projection, size) = run(&gpu, quality, 1.0);
        let (flipped, _) = run(&gpu, quality, -1.0);
        let base = bilinear(&render(size, [0.0, 0.0]), size);
        let (e_proj, e_flip, e_bilinear) = (
            rmse(&projection, &truth),
            rmse(&flipped, &truth),
            rmse(&base, &truth),
        );
        eprintln!(
            "{quality:?} render {size:?} -> {DISPLAY:?}: RMSE vs native  DLSS (projection sign) {e_proj:.4}  \
             DLSS (flipped sign) {e_flip:.4}  bilinear {e_bilinear:.4}"
        );
        assert!(
            e_proj.is_finite() && e_proj < 0.8 * e_bilinear,
            "DLSS is not clearly better than bilinear"
        );
        assert!(
            e_proj < e_flip,
            "the flipped jitter sign upscaled better: the seam's sign is wrong"
        );
    }
    eprintln!("VRAM held by DLSS: {:?} bytes", gpu.ngx.vram_bytes());
}

/// A device other than the NGX instance's is refused before NGX records anything (audit 11-03,
/// dlss M4): at creation, and at evaluation of a feature made on the right device.
#[test]
fn superresolution_refuses_a_device_other_than_the_ngx_device() {
    if std::env::var("REGIA_DLSS_SMOKE").as_deref() != Ok("1") {
        return;
    }
    let gpu = open();
    let (other, other_queue, other_config) = open_device();
    assert!(
        gpu.ngx.is_on(&gpu.device.clone()),
        "a clone keeps its device identity"
    );
    assert!(
        !gpu.ngx.is_on(&other),
        "independent instances have distinct Vulkan devices"
    );
    let other_ngx = Ngx::new(&other_config, &other).expect("independent NGX device");
    assert!(other_ngx.is_on(&other));
    assert!(!other_ngx.is_on(&gpu.device));
    let settings = gpu
        .ngx
        .optimal_settings(DISPLAY, PerfQuality::Quality)
        .expect("optimal settings");
    let flags = CreateFlags::default();
    let mismatch = |error: regia_dlss::DlssError| match error {
        regia_dlss::DlssError::Ngx { call, failure } => {
            assert_eq!(call, "device_mismatch");
            assert_eq!(failure, regia_dlss::NgxFailure::InvalidParameter);
        }
        other => panic!("expected device_mismatch, got {other:?}"),
    };
    let created = SuperResolution::new(
        &gpu.ngx,
        &other,
        &other_queue,
        DISPLAY,
        PerfQuality::Quality,
        flags,
        settings.max,
    );
    mismatch(created.expect_err("a feature on another device must be refused"));

    let mut sr = SuperResolution::new(
        &gpu.ngx,
        &gpu.device,
        &gpu.queue,
        DISPLAY,
        PerfQuality::Quality,
        flags,
        settings.max,
    )
    .expect("create the DLSS feature");
    let size = settings.render;
    let sampled = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST;
    let color = texture(&gpu, "color", size, wgpu::TextureFormat::Rgba16Float, sampled);
    let motion = texture(&gpu, "motion", size, wgpu::TextureFormat::Rg16Float, sampled);
    let depth = texture(
        &gpu,
        "depth",
        size,
        wgpu::TextureFormat::Depth32Float,
        wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
    );
    let output = texture(
        &gpu,
        "output",
        DISPLAY,
        wgpu::TextureFormat::Rgba16Float,
        wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
    );
    let views =
        [&color, &motion, &depth, &output].map(|t| t.create_view(&wgpu::TextureViewDescriptor::default()));
    let inputs = EvalInputs {
        color: &views[0],
        depth: &views[2],
        motion: &views[1],
        exposure: None,
        reactive: None,
        transparency: None,
        output: &views[3],
        jitter: [0.0, 0.0],
        motion_scale: [size[0] as f32, size[1] as f32],
        render_size: size,
        reset: true,
        frame_time_ms: 16.6,
    };
    let mut encoder = other.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    mismatch(
        sr.evaluate(&other, &mut encoder, &inputs)
            .expect_err("evaluating on another device must be refused"),
    );
}

/// The textures of one evaluation at `size` (color, motion, depth) and the display-size output.
fn eval_textures(gpu: &Gpu, size: [u32; 2]) -> [wgpu::Texture; 4] {
    let sampled = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST;
    [
        texture(gpu, "color", size, wgpu::TextureFormat::Rgba16Float, sampled),
        texture(gpu, "motion", size, wgpu::TextureFormat::Rg16Float, sampled),
        texture(
            gpu,
            "depth",
            size,
            wgpu::TextureFormat::Depth32Float,
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
        ),
        texture(
            gpu,
            "output",
            DISPLAY,
            wgpu::TextureFormat::Rgba16Float,
            wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
        ),
    ]
}

/// Audit 11-03, dlss H1: the feature is dropped between `evaluate` and the submission of its
/// buffer. The lease in `DlssCommands` keeps the feature alive until the buffer was submitted, so
/// NGX never runs on a released feature (before, the drop released it at once: the buffer was a
/// plain `wgpu::CommandBuffer`).
#[test]
fn a_feature_dropped_before_its_buffer_is_submitted_is_released_after_it() {
    if std::env::var("REGIA_DLSS_SMOKE").as_deref() != Ok("1") {
        return;
    }
    let gpu = open();
    let settings = gpu
        .ngx
        .optimal_settings(DISPLAY, PerfQuality::Quality)
        .expect("optimal settings");
    let mut sr = SuperResolution::new(
        &gpu.ngx,
        &gpu.device,
        &gpu.queue,
        DISPLAY,
        PerfQuality::Quality,
        CreateFlags::default(),
        settings.max,
    )
    .expect("create the DLSS feature");
    let size = settings.render;
    let textures = eval_textures(&gpu, size);
    let views = textures
        .each_ref()
        .map(|t| t.create_view(&wgpu::TextureViewDescriptor::default()));
    let inputs = EvalInputs {
        color: &views[0],
        depth: &views[2],
        motion: &views[1],
        exposure: None,
        reactive: None,
        transparency: None,
        output: &views[3],
        jitter: [0.0, 0.0],
        motion_scale: [size[0] as f32, size[1] as f32],
        render_size: size,
        reset: true,
        frame_time_ms: 16.6,
    };
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    let commands = sr.evaluate(&gpu.device, &mut encoder, &inputs).expect("evaluate");
    release(sr);
    commands.submit(&gpu.queue, encoder.finish());
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("the submission with the dropped feature completes");
}

/// Audit 11-03, dlss M1: two `Ngx` on one device share one NGX instance. Dropping the first no
/// longer shuts NGX down under the second (before, each `Ngx::new` initialised NGX again and
/// each drop called `Shutdown1` for the device).
#[test]
fn two_ngx_on_one_device_share_one_instance() {
    if std::env::var("REGIA_DLSS_SMOKE").as_deref() != Ok("1") {
        return;
    }
    let (device, queue, config) = open_device();
    let first = Ngx::new(&config, &device).expect("first Ngx");
    let second = Ngx::new(&config, &device).expect("second Ngx");
    release(first);
    let settings = second
        .optimal_settings(DISPLAY, PerfQuality::Quality)
        .expect("the second Ngx still works after the first dropped");
    let sr = SuperResolution::new(
        &second,
        &device,
        &queue,
        DISPLAY,
        PerfQuality::Quality,
        CreateFlags::default(),
        settings.max,
    )
    .expect("a feature on the shared instance");
    release(second);
    // The feature keeps NGX alive; it shuts down after the feature's release.
    release(sr);
    let again = Ngx::new(&config, &device).expect("NGX initialises again after the shutdown");
    again
        .optimal_settings(DISPLAY, PerfQuality::Quality)
        .expect("the re-initialised instance works");
}

/// Audit 11-03, dlss M2 and L1: the extension gate asks NGX about this device. The hooked device
/// passes; a plain device on the same GPU (no NGX extensions) is refused, whatever lists the
/// process-wide status holds.
#[test]
fn the_extension_gate_is_per_device() {
    if std::env::var("REGIA_DLSS_SMOKE").as_deref() != Ok("1") {
        return;
    }
    let (device, _queue, config) = open_device();
    assert!(
        regia_dlss::device_has_ngx_extensions(&device, &config).expect("ask NGX"),
        "the hooked device enables what NGX asks for"
    );
    let descriptor = wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    };
    let plain = wgpu::Instance::new(descriptor);
    let adapters = regia_core::tasks::block_on(plain.enumerate_adapters(wgpu::Backends::VULKAN));
    let adapter = adapters
        .into_iter()
        .find(|a| a.get_info().vendor == 0x10DE)
        .expect("an NVIDIA adapter");
    let (plain_device, _plain_queue) =
        regia_core::tasks::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .expect("a plain device");
    assert!(
        !regia_dlss::device_has_ngx_extensions(&plain_device, &config).expect("ask NGX"),
        "a device without NGX's extensions is refused"
    );
}

#[test]
fn ngx_reports_a_code_instead_of_panicking_without_a_runtime_directory() {
    if std::env::var("REGIA_DLSS_SMOKE").as_deref() != Ok("1") {
        return;
    }
    let (device, _queue, _config) = open_device();
    // No runtime directory: either NGX finds the runtime on its own (driver or update cache)
    // or it returns a code. Never a panic.
    let empty = NgxConfig::new(Vec::new());
    match Ngx::new(&empty, &device) {
        Ok(_) => eprintln!("NGX found a DLSS runtime without a directory"),
        Err(error) => eprintln!("without a runtime directory: {} ({error})", error.code()),
    }
}
