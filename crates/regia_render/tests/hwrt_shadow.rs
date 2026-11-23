// SPDX-License-Identifier: MPL-2.0
//! Hardware ray tracing prep (ADR-515): shadow rays on the GPU.
//!
//! The compute fallback runs on every adapter (lavapipe in CI) and must match both the CPU
//! reference and the analytic shadow of a floating occluder, sample for sample. The ray-query
//! comparison needs an adapter with ray queries (lavapipe on Mesa 24.1 and later has them): it runs
//! when the adapter offers them and prints `skipped: no ray queries on <adapter>` otherwise
//! (`regia_test_utils::hwrt`).

use regia_render::hwrt::{
    AccelInstance, AccelMesh, AccelPlanner, GpuAccel, GpuAccelMesh, HwrtReason, HwrtRequest, RtShadowRequest,
    ShadowBvh, ShadowFallbackPass, ShadowFallbackScene, ShadowParams, ShadowRayQueryPass, ShadowTargets,
    SunShadowReason, SunShadowVariant, create_shadow_visibility, last_device_report, probe_adapters,
    select_sun_shadow_variant, trace_shadow_rays_cpu, upload_shadow_origins,
};
use regia_render::{RenderDevice, RenderProfilePreset};
use wgpu::util::DeviceExt as _;

/// Occluder: a 2 × 2 quad at y = 2 over x, z in [-1, 1], split into a 10 × 10 grid of quads
/// (200 triangles) so the BVH has depth.
const OCCLUDER_CELLS: usize = 10;
const OCCLUDER_Y: f32 = 2.0;

fn device() -> RenderDevice {
    // One device per process: parallel test threads must not each open one (RenderDevice::shared_headless).
    match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// `last_device_report` is process-wide: the latest device created records it. The ray-query test
/// creates a device that asks for ray queries, so every test that reads the report holds this lock,
/// and creates its own default device right before reading it.
static REPORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn report_guard() -> std::sync::MutexGuard<'static, ()> {
    REPORT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A default device made just now, so `last_device_report` describes it (hold `report_guard`).
fn device_with_fresh_report() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn occluder() -> Vec<[[f32; 3]; 3]> {
    let step = 2.0 / OCCLUDER_CELLS as f32;
    let mut tris = Vec::new();
    for i in 0..OCCLUDER_CELLS {
        for j in 0..OCCLUDER_CELLS {
            let x0 = -1.0 + step * i as f32;
            let z0 = -1.0 + step * j as f32;
            let (x1, z1) = (x0 + step, z0 + step);
            tris.push([[x0, OCCLUDER_Y, z0], [x1, OCCLUDER_Y, z0], [x1, OCCLUDER_Y, z1]]);
            tris.push([[x0, OCCLUDER_Y, z0], [x1, OCCLUDER_Y, z1], [x0, OCCLUDER_Y, z1]]);
        }
    }
    tris
}

/// Decoys under the ground (y in [-6.5, -2.5]): in the tree, never in an upward ray's way.
fn decoys() -> Vec<[[f32; 3]; 3]> {
    let mut tris = Vec::new();
    for i in 0..12 {
        for j in 0..12 {
            let x = -6.0 + i as f32;
            let z = -6.0 + j as f32;
            let y = -3.0 - ((i * 7 + j * 3) % 4) as f32;
            tris.push([[x, y, z], [x + 0.5, y + 0.5, z], [x, y - 0.5, z + 0.5]]);
        }
    }
    tris
}

/// Ground samples on a grid offset from the occluder's cell edges and diagonals by at least
/// 0.0125 (x and z use different offsets), so no ray grazes an edge and every answer is exact.
fn samples() -> Vec<[f32; 3]> {
    let mut out = Vec::new();
    for i in 0..19 {
        for k in 0..20 {
            out.push([-2.375 + 0.25 * i as f32, 0.0, -2.4375 + 0.25 * k as f32]);
        }
    }
    out
}

/// The analytic answer: a ray toward `light` from `p` crosses y = 2 at `p.x + shift_x`, where
/// `shift_x` is the light's horizontal run per 2 units of rise (0 straight up, 1 for (1, 2, 0)).
fn expected(points: &[[f32; 3]], shift_x: f32) -> Vec<f32> {
    points
        .iter()
        .map(|p| {
            let shadowed = (p[0] + shift_x).abs() < 1.0 && p[2].abs() < 1.0;
            if shadowed { 0.0 } else { 1.0 }
        })
        .collect()
}

fn read_f32s(
    device: &RenderDevice,
    mut encoder: wgpu::CommandEncoder,
    source: &wgpu::Buffer,
    count: usize,
) -> Vec<f32> {
    let size = (count.max(1) * 4) as u64;
    let staging = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("hwrt readback"),
        size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_buffer_to_buffer(source, 0, &staging, 0, size);
    device.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    staging.slice(..).map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    device
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll readback");
    rx.recv().expect("map callback").expect("map readback");
    let mapped = staging.slice(..).get_mapped_range().expect("mapped range");
    let (words, _) = mapped.as_chunks::<4>();
    let values: Vec<f32> = words.iter().take(count).map(|w| f32::from_le_bytes(*w)).collect();
    drop(mapped);
    staging.unmap();
    values
}

fn mismatches(got: &[f32], want: &[f32], points: &[[f32; 3]]) -> Vec<String> {
    got.iter()
        .zip(want)
        .zip(points)
        .filter(|((g, w), _)| g != w)
        .map(|((g, w), p)| format!("{p:?}: got {g}, want {w}"))
        .collect()
}

#[test]
fn the_compute_fallback_matches_the_cpu_reference_and_the_analytic_shadow() {
    let device = device();
    let mut tris = occluder();
    tris.extend(decoys());
    let bvh = ShadowBvh::build(&tris).expect("bvh");
    assert!(
        bvh.nodes().len() > 64,
        "the tree has depth: {} nodes",
        bvh.nodes().len()
    );
    let points = samples();
    let pass = ShadowFallbackPass::new(&device.device);
    let scene = ShadowFallbackScene::upload(&device.device, &bvh);
    let origins = upload_shadow_origins(&device.device, &points);
    for (light, shift_x) in [([0.0, 1.0, 0.0], 0.0), ([1.0, 2.0, 0.0], 1.0)] {
        let params = ShadowParams::toward(light).expect("params");
        let want = expected(&points, shift_x);
        let shadowed = want.iter().filter(|&&v| v == 0.0).count();
        assert!(
            shadowed > 40 && shadowed < points.len() - 40,
            "{shadowed} shadowed samples"
        );

        let cpu = trace_shadow_rays_cpu(&bvh, &params, &points);
        assert!(
            mismatches(&cpu, &want, &points).is_empty(),
            "cpu {light:?}: {:?}",
            mismatches(&cpu, &want, &points)
        );

        let visibility = create_shadow_visibility(&device.device, points.len() as u32);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("hwrt fallback test"),
            });
        pass.encode(
            &device.device,
            &mut encoder,
            &scene,
            &params,
            &ShadowTargets {
                origins: &origins,
                visibility: &visibility,
                sample_count: points.len() as u32,
            },
        );
        let gpu = read_f32s(&device, encoder, &visibility, points.len());
        let wrong = mismatches(&gpu, &cpu, &points);
        assert!(
            wrong.is_empty(),
            "gpu vs cpu {light:?}: {} of {}: {wrong:?}",
            wrong.len(),
            points.len()
        );
    }
}

#[test]
fn an_empty_scene_lights_every_sample_on_the_gpu() {
    let device = device();
    let bvh = ShadowBvh::build(&[]).expect("empty bvh");
    let points = samples();
    let params = ShadowParams::toward([0.3, 1.0, -0.2]).expect("params");
    let pass = ShadowFallbackPass::new(&device.device);
    let scene = ShadowFallbackScene::upload(&device.device, &bvh);
    let origins = upload_shadow_origins(&device.device, &points);
    let visibility = create_shadow_visibility(&device.device, points.len() as u32);
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("hwrt empty test"),
        });
    pass.encode(
        &device.device,
        &mut encoder,
        &scene,
        &params,
        &ShadowTargets {
            origins: &origins,
            visibility: &visibility,
            sample_count: points.len() as u32,
        },
    );
    let gpu = read_f32s(&device, encoder, &visibility, points.len());
    assert!(gpu.iter().all(|&v| v == 1.0), "{gpu:?}");
}

#[test]
fn the_device_report_matches_the_device_and_gates_the_builders() {
    let _guard = report_guard();
    let device = device_with_fresh_report();
    let report = last_device_report().expect("RenderDevice::new records a report");
    let has_rq = device
        .device
        .features()
        .contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY);
    assert_eq!(report.device_ray_query, has_rq);
    if has_rq {
        assert_eq!(report.reason(), HwrtReason::Enabled);
        assert!(GpuAccel::new(&device.device).is_ok());
        assert!(ShadowRayQueryPass::new(&device.device).is_ok());
    } else {
        // The default: no profile asked, or an adapter without ray queries. Neither builder can
        // be constructed, so no code path reaches the experimental API.
        assert_ne!(report.reason(), HwrtReason::Enabled);
        assert_eq!(
            GpuAccel::new(&device.device).map(|_| ()).map_err(|e| e.code()),
            Err("accel_ray_query_disabled")
        );
        assert_eq!(
            ShadowRayQueryPass::new(&device.device)
                .map(|_| ())
                .map_err(|e| e.code()),
            Err("accel_ray_query_disabled")
        );
    }
}

/// The machine-wide probe (`regia doctor`, `render.hwrt {"adapters": true}`, ADR-1149) lists
/// the adapters `list_adapters` numbers for `REGIA_GPU_INDEX`, in the same order.
#[test]
fn the_adapter_probe_lists_what_list_adapters_lists() {
    let probed = probe_adapters();
    let listed = regia_render::list_adapters();
    assert_eq!(probed.len(), listed.len());
    for (p, l) in probed.iter().zip(&listed) {
        assert_eq!(p.index, l.index);
        assert_eq!(p.name, l.name);
        assert_eq!(p.probe.backend, l.backend);
        assert_eq!(p.device_type, l.device_type);
        // Asking is what a device would do; refusing must name a reason other than "off".
        let reason = p.decide(HwrtRequest::On).reason;
        assert_ne!(reason, HwrtReason::NotRequested);
        assert_eq!(
            reason == HwrtReason::Enabled,
            p.probe.ray_query && p.probe.limits.is_usable()
        );
    }
}

/// The sun-shadow variant chosen for this device (ADR-1149, ADR-2035) is one the device can run,
/// and it is the ray-query variant exactly when the device has ray queries.
#[test]
fn the_sun_shadow_variant_agrees_with_the_device() {
    let _guard = report_guard();
    let device = device_with_fresh_report();
    let report = last_device_report().expect("RenderDevice::new records a report");
    let features = device.device.features();
    let has_rq = features.contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY);
    let ultra = Some(RenderProfilePreset::DesktopUltra);
    for request in RtShadowRequest::ALL {
        let s = select_sun_shadow_variant(request, Some(&report), ultra);
        assert!(s.variant.runs_on(features), "{request:?} -> {:?}", s.variant);
    }
    let on = select_sun_shadow_variant(RtShadowRequest::On, Some(&report), ultra);
    assert_eq!(on.variant == SunShadowVariant::RayQuery, has_rq);
    if !has_rq {
        assert_eq!(on.reason, SunShadowReason::ScreenFallback);
    }
}

fn blas_vertices(device: &wgpu::Device, tris: &[[[f32; 3]; 3]]) -> wgpu::Buffer {
    let flat: Vec<[f32; 3]> = tris.iter().flatten().copied().collect();
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("hwrt test blas vertices"),
        contents: bytemuck::cast_slice(&flat),
        usage: wgpu::BufferUsages::BLAS_INPUT,
    })
}

#[test]
fn ray_query_shadows_match_the_compute_fallback() {
    let _guard = report_guard();
    let Some(device) = regia_test_utils::hwrt::ray_query_device() else {
        return;
    };
    let d = &device.device;
    // Occluder non-indexed, decoys indexed: both BLAS input shapes.
    let occ = occluder();
    let dec = decoys();
    let occ_vertices = blas_vertices(d, &occ);
    let dec_vertices = blas_vertices(d, &dec);
    let dec_indices: Vec<u32> = (0..(dec.len() * 3) as u32).collect();
    let dec_index_buffer = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("hwrt test blas indices"),
        contents: bytemuck::cast_slice(&dec_indices),
        usage: wgpu::BufferUsages::BLAS_INPUT,
    });
    let meshes = [
        AccelMesh {
            key: 1,
            generation: 1,
            vertex_count: (occ.len() * 3) as u32,
            index_count: 0,
        },
        AccelMesh {
            key: 2,
            generation: 1,
            vertex_count: (dec.len() * 3) as u32,
            index_count: dec_indices.len() as u32,
        },
    ];
    let buffers = [
        GpuAccelMesh {
            key: 1,
            vertex_buffer: &occ_vertices,
            first_vertex: 0,
            vertex_stride: 12,
            index_buffer: None,
            first_index: 0,
            opaque: true,
        },
        GpuAccelMesh {
            key: 2,
            vertex_buffer: &dec_vertices,
            first_vertex: 0,
            vertex_stride: 12,
            index_buffer: Some(&dec_index_buffer),
            first_index: 0,
            opaque: true,
        },
    ];
    let identity = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    let mut far = identity;
    far[3][0] = 50.0;
    let instances = [
        AccelInstance {
            mesh: 1,
            transform: identity,
            custom_data: 0,
            mask: 0xFF,
        },
        // A second placement of the occluder far away: instancing, no effect on the samples.
        AccelInstance {
            mesh: 1,
            transform: far,
            custom_data: 1,
            mask: 0xFF,
        },
        AccelInstance {
            mesh: 2,
            transform: identity,
            custom_data: 2,
            mask: 0xFF,
        },
    ];
    let mut accel = GpuAccel::new(d).expect("ray-query device");
    let pass = ShadowRayQueryPass::new(d).expect("ray-query pass");
    let points = samples();
    let origins = upload_shadow_origins(d, &points);
    let mut bvh_tris = occ;
    bvh_tris.extend(dec.iter().copied());
    let bvh = ShadowBvh::build(&bvh_tris).expect("bvh");
    for (round, (light, shift_x)) in [([0.0, 1.0, 0.0], 0.0), ([1.0, 2.0, 0.0], 1.0)]
        .into_iter()
        .enumerate()
    {
        let params = ShadowParams::toward(light).expect("params");
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("hwrt rq test"),
        });
        let stats = accel
            .build(d, &mut encoder, &meshes, &buffers, &instances)
            .expect("build");
        // The first build makes both BLAS; the second reuses them and only rebuilds the TLAS.
        assert_eq!(stats.blas_built, if round == 0 { 2 } else { 0 });
        assert_eq!(stats.instances, 3);
        let tlas = accel.tlas().expect("tlas");
        let visibility = create_shadow_visibility(d, points.len() as u32);
        pass.encode(
            d,
            &mut encoder,
            tlas,
            &params,
            &ShadowTargets {
                origins: &origins,
                visibility: &visibility,
                sample_count: points.len() as u32,
            },
        );
        let gpu = read_f32s(&device, encoder, &visibility, points.len());
        let want = expected(&points, shift_x);
        let wrong = mismatches(&gpu, &want, &points);
        assert!(wrong.is_empty(), "ray query vs analytic {light:?}: {wrong:?}");
        let cpu = trace_shadow_rays_cpu(&bvh, &params, &points);
        assert_eq!(gpu, cpu, "ray query vs the fallback's reference");
    }
    assert_eq!(accel.planner().built_count(), 2);
    // The planner alone agrees: nothing left to build.
    let replan = AccelPlanner::new().plan(
        &meshes,
        &instances,
        &regia_render::hwrt::AccelLimits::from_limits(&d.limits()),
    );
    assert_eq!(
        replan.map(|p| p.builds()),
        Ok(2),
        "a fresh planner builds both meshes"
    );
}
