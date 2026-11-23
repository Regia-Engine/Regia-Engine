// SPDX-License-Identifier: MPL-2.0
//! The GPU many-light sampler against the CPU reference on the synthetic 1,000-light scene
//! (ADR-2470), on lavapipe in CI: the brute-force view matches the CPU brute force, the estimate
//! is unbiased and its error falls as the history builds, its error matches the CPU
//! estimator's, and steady frames create nothing and allocate nothing in the sampler's own code.
//! The numbers are printed (`--nocapture`).
//!
//! Its own test binary: it installs a counting global allocator.

use regia_render::restir_di::cpu::{self, Candidates, ClusterLists, ReferenceSampler};
use regia_render::restir_di::scene::SyntheticScene;
use regia_render::restir_di::{
    ManyLight, RestirClusters, RestirDiGpu, RestirDiSettings, RestirInputs, luminance,
};
use regia_render::{RenderDevice, TrackedDevice, frame_path_counters};

use regia_test_utils::alloc_counter::{self, CountingAlloc};

// The shared counting allocator (ADR-3291): this thread's requests.
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

/// This thread's allocation requests so far.
fn allocs() -> u64 {
    alloc_counter::thread_allocs()
}

const W: u32 = 64;
const H: u32 = 36;
const LIGHTS: u32 = 1000;

fn device() -> Option<RenderDevice> {
    match RenderDevice::shared_headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skipping: no GPU adapter (lavapipe) available: {err}");
            None
        }
    }
}

fn f16_to_f32(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = i32::from((h >> 10) & 0x1f);
    let frac = f32::from(h & 0x3ff);
    match exp {
        0 => sign * frac * 2f32.powi(-24),
        31 => sign * f32::INFINITY,
        _ => sign * (1.0 + frac / 1024.0) * 2f32.powi(exp - 15),
    }
}

/// The GPU side of one scene: uploaded G-buffer, lights and cluster lists.
struct Uploaded {
    _textures: Vec<wgpu::Texture>,
    views: [wgpu::TextureView; 4],
    lights: wgpu::Buffer,
    counts: wgpu::Buffer,
    indices: wgpu::Buffer,
    readback: wgpu::Buffer,
}

fn texture(
    device: &RenderDevice,
    format: wgpu::TextureFormat,
    texel: u32,
    bytes: &[u8],
) -> (wgpu::Texture, wgpu::TextureView) {
    let t = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("restir test input"),
        size: wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    device.queue.write_texture(
        t.as_image_copy(),
        bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(W * texel),
            rows_per_image: Some(H),
        },
        wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
    );
    let v = t.create_view(&wgpu::TextureViewDescriptor::default());
    (t, v)
}

fn storage(device: &RenderDevice, bytes: &[u8]) -> wgpu::Buffer {
    let b = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("restir test storage"),
        size: (bytes.len() as u64).max(64),
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    device.queue.write_buffer(&b, 0, bytes);
    b
}

fn upload(device: &RenderDevice, scene: &SyntheticScene, clusters: &ClusterLists) -> Uploaded {
    let g = &scene.gbuffer;
    let normal: Vec<f32> = (0..g.len())
        .flat_map(|i| [g.normal[i][0], g.normal[i][1], g.normal[i][2], g.roughness[i]])
        .collect();
    let albedo: Vec<f32> = (0..g.len())
        .flat_map(|i| [g.albedo[i][0], g.albedo[i][1], g.albedo[i][2], g.metallic[i]])
        .collect();
    let motion: Vec<f32> = g.motion.iter().flat_map(|m| [m[0], m[1], 0.0, 0.0]).collect();
    let (t0, v0) = texture(
        device,
        wgpu::TextureFormat::R32Float,
        4,
        bytemuck::cast_slice(&g.depth),
    );
    let (t1, v1) = texture(
        device,
        wgpu::TextureFormat::Rgba32Float,
        16,
        bytemuck::cast_slice(&normal),
    );
    let (t2, v2) = texture(
        device,
        wgpu::TextureFormat::Rgba32Float,
        16,
        bytemuck::cast_slice(&albedo),
    );
    let (t3, v3) = texture(
        device,
        wgpu::TextureFormat::Rgba32Float,
        16,
        bytemuck::cast_slice(&motion),
    );
    let readback = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("restir test readback"),
        size: u64::from(W * 8 * H),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    Uploaded {
        _textures: vec![t0, t1, t2, t3],
        views: [v0, v1, v2, v3],
        lights: storage(device, bytemuck::cast_slice::<ManyLight, u8>(&scene.lights)),
        counts: storage(device, bytemuck::cast_slice(&clusters.counts)),
        indices: storage(device, bytemuck::cast_slice(&clusters.indices)),
        readback,
    }
}

fn inputs<'a>(
    up: &'a Uploaded,
    scene: &SyntheticScene,
    clusters: &ClusterLists,
    reference: bool,
) -> RestirInputs<'a> {
    RestirInputs {
        size: (W, H),
        depth: &up.views[0],
        normal_roughness: &up.views[1],
        albedo_metallic: &up.views[2],
        motion: &up.views[3],
        lights: &up.lights,
        light_count: scene.lights.len() as u32,
        clusters: Some(RestirClusters {
            counts: &up.counts,
            indices: &up.indices,
            max_per_cluster: clusters.max_per_cluster,
        }),
        camera: scene.camera,
        reference,
    }
}

/// One frame recorded, submitted and read back.
fn run_frame(
    device: &RenderDevice,
    tracked: &TrackedDevice,
    sampler: &mut RestirDiGpu,
    up: &Uploaded,
    inputs: &RestirInputs<'_>,
) -> Vec<[f32; 3]> {
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("restir test"),
        });
    sampler
        .record(tracked, &device.queue, &mut encoder, inputs)
        .expect("record");
    let Some(out) = sampler.output_texture() else {
        panic!("no output after a frame");
    };
    encoder.copy_texture_to_buffer(
        out.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &up.readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(W * 8),
                rows_per_image: Some(H),
            },
        },
        wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
    );
    device.queue.submit([encoder.finish()]);
    let slice = up.readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    device
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let data = slice.get_mapped_range().expect("mapped range");
    let halves: &[u16] = bytemuck::cast_slice(&data);
    let (texels, _) = halves.as_chunks::<4>();
    let image = texels
        .iter()
        .map(|c| [f16_to_f32(c[0]), f16_to_f32(c[1]), f16_to_f32(c[2])])
        .collect();
    drop(data);
    up.readback.unmap();
    image
}

fn scene() -> (SyntheticScene, ClusterLists) {
    let scene = SyntheticScene::new(W, H, LIGHTS, 7);
    let clusters = ClusterLists::build(&scene.camera, W as f32 / H as f32, &scene.lights, 256);
    assert_eq!(clusters.overflowed, 0);
    (scene, clusters)
}

#[test]
fn the_reference_view_matches_the_cpu_brute_force() {
    let Some(device) = device() else { return };
    let tracked = TrackedDevice::observe(&device.device);
    let (scene, clusters) = scene();
    let up = upload(&device, &scene, &clusters);
    let mut sampler = RestirDiGpu::new(&tracked, RestirDiSettings::default(), 1).expect("sampler");
    let gpu = run_frame(
        &device,
        &tracked,
        &mut sampler,
        &up,
        &inputs(&up, &scene, &clusters, true),
    );
    let truth = cpu::brute_force(&scene.gbuffer, &scene.camera, &scene.lights);
    let mean = cpu::mean_luminance(&truth);
    let mut worst = 0.0f64;
    for (a, b) in gpu.iter().zip(&truth) {
        let d = f64::from((luminance(*a) - luminance(*b)).abs());
        worst = worst.max(d / (f64::from(luminance(*b)) + 0.05 * mean));
    }
    println!("reference view: mean {mean}, worst relative difference {worst}");
    assert!(mean > 0.0);
    assert!(worst < 5e-3, "GPU brute force differs from the CPU's by {worst}");
}

#[test]
fn the_estimate_is_unbiased_converges_and_matches_the_cpu_error() {
    let Some(device) = device() else { return };
    let tracked = TrackedDevice::observe(&device.device);
    let (scene, clusters) = scene();
    let up = upload(&device, &scene, &clusters);
    let truth = cpu::brute_force(&scene.gbuffer, &scene.camera, &scene.lights);
    let truth_mean = cpu::mean_luminance(&truth);
    let input = inputs(&up, &scene, &clusters, false);
    const FRAMES: usize = 6;
    const SEEDS: u32 = 48;
    let mut means = Vec::new();
    let mut gpu_rmse = [0.0f64; FRAMES];
    let mut cpu_rmse = [0.0f64; FRAMES];
    let mut running = vec![[0.0f32; 3]; truth.len()];
    for seed in 0..SEEDS {
        let mut sampler = RestirDiGpu::new(&tracked, RestirDiSettings::default(), seed).expect("sampler");
        let mut reference = ReferenceSampler::new(RestirDiSettings::default(), seed).expect("cpu sampler");
        let mut last = Vec::new();
        for (g, c) in gpu_rmse.iter_mut().zip(cpu_rmse.iter_mut()) {
            last = run_frame(&device, &tracked, &mut sampler, &up, &input);
            *g += cpu::relative_rmse(&last, &truth);
            let frame = reference
                .frame(
                    &scene.gbuffer,
                    &scene.camera,
                    &scene.lights,
                    Candidates::Clusters(&clusters),
                )
                .expect("cpu frame");
            *c += cpu::relative_rmse(&frame.radiance, &truth);
        }
        for (r, v) in running.iter_mut().zip(&last) {
            for (a, b) in r.iter_mut().zip(v) {
                *a += b / SEEDS as f32;
            }
        }
        means.push(cpu::mean_luminance(&last));
    }
    let n = f64::from(SEEDS);
    let mean = means.iter().sum::<f64>() / n;
    let se = (means.iter().map(|m| (m - mean).powi(2)).sum::<f64>() / (n - 1.0) / n).sqrt();
    let gpu_rmse = gpu_rmse.map(|v| v / n);
    let cpu_rmse = cpu_rmse.map(|v| v / n);
    let averaged = cpu::relative_rmse(&running, &truth);
    println!(
        "gpu estimate: truth {truth_mean}, mean {mean} ± {se} ({:.2} sigma); rmse per frame gpu {gpu_rmse:?} cpu {cpu_rmse:?}; {SEEDS}-seed average rmse {averaged}",
        (mean - truth_mean).abs() / se
    );
    assert!(
        (mean - truth_mean).abs() < 4.0 * se.max(1e-9),
        "biased: {} sigma",
        (mean - truth_mean).abs() / se
    );
    assert!(
        (mean - truth_mean).abs() < 0.02 * truth_mean,
        "relative bias {}",
        (mean - truth_mean) / truth_mean
    );
    assert!(
        gpu_rmse[FRAMES - 1] < 0.8 * gpu_rmse[0],
        "the error must fall as the history builds: {gpu_rmse:?}"
    );
    for (f, (g, c)) in gpu_rmse.iter().zip(&cpu_rmse).enumerate() {
        assert!(
            (0.85..1.15).contains(&(g / c)),
            "frame {f}: gpu rmse {g} vs cpu {c}"
        );
    }
    assert!(
        averaged < gpu_rmse[FRAMES - 1],
        "averaging seeds converges to the brute force"
    );
}

#[test]
fn steady_frames_create_nothing_and_the_sampler_allocates_nothing() {
    let Some(device) = device() else { return };
    let tracked = TrackedDevice::observe(&device.device);
    let (scene, clusters) = scene();
    let up = upload(&device, &scene, &clusters);
    let input = inputs(&up, &scene, &clusters, false);
    let mut sampler = RestirDiGpu::new(&tracked, RestirDiSettings::default(), 9).expect("sampler");
    for _ in 0..2 {
        run_frame(&device, &tracked, &mut sampler, &up, &input);
    }
    let builds = (sampler.resource_builds(), sampler.bind_group_builds());
    let before = frame_path_counters();
    let mut own = 0;
    for _ in 0..8 {
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("steady"),
            });
        let a = allocs();
        sampler.prepare(&tracked, &input).expect("prepare");
        own += allocs() - a;
        let (buffer, bytes) = sampler.params_upload();
        device.queue.write_buffer(buffer, 0, bytes);
        sampler.encode(&mut encoder);
        device.queue.submit([encoder.finish()]);
    }
    let after = frame_path_counters().since(before);
    println!("steady: {after:?}, sampler allocations {own}");
    assert_eq!(own, 0, "prepare allocates once warm");
    assert_eq!((sampler.resource_builds(), sampler.bind_group_builds()), builds);
    assert_eq!(after.pipelines, 0);
    assert_eq!(after.bind_groups, 0);
    assert_eq!(after.buffers, 0);
    assert_eq!(after.textures, 0);
    assert_eq!(sampler.frames(), 10);
}

#[test]
fn a_new_size_rebuilds_once_and_drops_the_history() {
    let Some(device) = device() else { return };
    let tracked = TrackedDevice::observe(&device.device);
    let (scene, clusters) = scene();
    let up = upload(&device, &scene, &clusters);
    let mut sampler = RestirDiGpu::new(&tracked, RestirDiSettings::default(), 2).expect("sampler");
    let input = inputs(&up, &scene, &clusters, false);
    run_frame(&device, &tracked, &mut sampler, &up, &input);
    assert!(sampler.has_history());
    let mut smaller = input;
    smaller.size = (W / 2, H / 2);
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("resize"),
        });
    sampler.prepare(&tracked, &smaller).expect("prepare");
    assert!(!sampler.has_history(), "a new size starts a new history");
    assert_eq!(sampler.resource_builds(), 2);
    sampler.encode(&mut encoder);
    device.queue.submit([encoder.finish()]);
    let mut bad = input;
    bad.light_count = 10_000;
    let err = sampler.prepare(&tracked, &bad).expect_err("too many lights");
    assert_eq!(err.code(), "bad_input");
}

#[test]
fn the_screen_trace_darkens_occluded_light_and_stays_finite() {
    let Some(device) = device() else { return };
    let tracked = TrackedDevice::observe(&device.device);
    let (scene, clusters) = scene();
    let up = upload(&device, &scene, &clusters);
    let input = inputs(&up, &scene, &clusters, false);
    let mean_of = |settings: RestirDiSettings| {
        let mut sum = 0.0;
        for seed in 0..8 {
            let mut sampler = RestirDiGpu::new(&tracked, settings.clone(), seed).expect("sampler");
            let mut last = Vec::new();
            for _ in 0..3 {
                last = run_frame(&device, &tracked, &mut sampler, &up, &input);
            }
            assert!(
                last.iter().flatten().all(|v| v.is_finite() && *v >= 0.0),
                "finite radiance"
            );
            sum += cpu::mean_luminance(&last) / 8.0;
        }
        sum
    };
    let open = mean_of(RestirDiSettings::default());
    let traced = mean_of(RestirDiSettings {
        trace_steps: 16,
        ..RestirDiSettings::default()
    });
    println!("screen trace: unshadowed {open}, traced {traced}");
    assert!(traced < open, "the boxes and the wall shadow some lights");
    assert!(traced > 0.5 * open, "most of the floor still sees its lights");
}
