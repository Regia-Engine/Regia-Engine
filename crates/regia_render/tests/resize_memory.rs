// SPDX-License-Identifier: MPL-2.0
//! Resizing the main view returns its GPU memory (v0.1.6 review fix).
//!
//! DO27 on an RX 7900 (v0.1.4): while a texture view flipped the window's view state every
//! frame, `vram_allocated` climbed from 2.7 to 17.5 GB in about 20 s and stayed there after the
//! flipping stopped. Window resizes, fullscreen toggles and dynamic resolution re-create the
//! same size-dependent targets, so the main view alone must not keep the old ones either.
//!
//! The test renders Desktop High (TAAU, SSAO, SSR, SSGI, contact shadows, paged sun shadows,
//! sky, fog and clouds) at a base size, then alternates the output between a large and a small
//! size, then drags it through a run of sizes (a window resize), then returns to the base
//! size. The allocator's live bytes (`memory_report`, Vulkan and D3D12) and the renderer's
//! texture report (`texture_memory`) must come back to within a small margin of the baseline.

use std::collections::HashMap;

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, FrameTarget, LabMaterial, LabMesh, LabScene,
    LabVertex, MemoryReport, OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
    TimeOfDay,
};

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
/// The base (window) size.
const BASE: (u32, u32) = (640, 360);
/// The other size of the alternation (a 512² texture view's old flip).
const SMALL: (u32, u32) = (512, 512);
/// The large size of the alternation (a fullscreen toggle).
const LARGE: (u32, u32) = (1280, 720);
/// Allocation growth tolerated after the churn (staging and readback pools settle).
const MARGIN_BYTES: u64 = 4 << 20;

/// Label prefix of wgpu's own indirect-draw validation buffers. They live in a free list that
/// grows to the peak number of draws in flight and never shrinks (wgpu-core `indirect_validation`),
/// so their size follows queue depth and timing, not the window size: ADR-1564.
const WGPU_VALIDATION_POOL: &str = "(wgpu internal) Indirect draw validation";

/// Live bytes of wgpu's validation buffer pool in `report`.
fn validation_pool_bytes(report: &MemoryReport) -> u64 {
    report
        .labels
        .iter()
        .filter(|l| l.label.starts_with(WGPU_VALIDATION_POOL))
        .map(|l| l.bytes)
        .sum()
}

/// Allocated bytes without that pool, the figure the resize check compares.
fn allocated_without_pool(report: &MemoryReport) -> u64 {
    report
        .allocated_bytes
        .saturating_sub(validation_pool_bytes(report))
}

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip resize_memory: {err}");
            None
        }
    }
}

fn scene() -> LabScene {
    let (b, bi) = regia_render::box_mesh([0.6, 0.6, 0.6]);
    let floor = [
        LabVertex::from_pos_normal_uv([-20.0, 0.0, -20.0], [0.0, 1.0, 0.0], [0.0, 0.0]),
        LabVertex::from_pos_normal_uv([20.0, 0.0, -20.0], [0.0, 1.0, 0.0], [1.0, 0.0]),
        LabVertex::from_pos_normal_uv([20.0, 0.0, 20.0], [0.0, 1.0, 0.0], [1.0, 1.0]),
        LabVertex::from_pos_normal_uv([-20.0, 0.0, 20.0], [0.0, 1.0, 0.0], [0.0, 1.0]),
    ];
    LabScene {
        width: BASE.0,
        height: BASE.1,
        eye: [0.0, 2.0, 7.0],
        target: [0.0, 0.8, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.5,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 14.5,
            auto_exposure: true,
            bloom: true,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([0.35, -0.8, -0.45], [1.0, 0.97, 0.92], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.5, 0.5, 0.5, 1.0],
                roughness: 0.6,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.8, 0.8, 0.8, 1.0],
                roughness: 0.15,
                metallic: 1.0,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![
            LabMesh {
                vertices: floor.to_vec(),
                indices: vec![0, 2, 1, 0, 3, 2],
                material: 1,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: regia_render::translate_mesh(&b, [-1.5, 0.6, 0.0]),
                indices: bi.clone(),
                material: 0,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: regia_render::translate_mesh(&b, [1.8, 0.6, -1.0]),
                indices: bi,
                material: 0,
                ..LabMesh::unskinned()
            },
        ],
    }
}

fn desktop_high() -> FrameQuality {
    FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh))
}

struct Outputs {
    targets: HashMap<(u32, u32), (wgpu::Texture, wgpu::TextureView)>,
}

impl Outputs {
    /// Every output texture the test uses, created up front so they sit in the baseline.
    fn new(device: &RenderDevice, sizes: &[(u32, u32)]) -> Self {
        let mut targets = HashMap::new();
        for &(w, h) in sizes {
            targets.entry((w, h)).or_insert_with(|| {
                let texture = device.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("resize test output"),
                    size: wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: FORMAT,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                    view_formats: &[],
                });
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                (texture, view)
            });
        }
        Self { targets }
    }
}

/// One main-view frame at `size`. The camera sways between two positions so the screen-space
/// passes run instead of holding their last result.
fn frame(
    renderer: &mut FrameRenderer,
    device: &RenderDevice,
    lab: &LabScene,
    outputs: &Outputs,
    size: (u32, u32),
    n: u32,
) {
    let mut scene = lab.as_frame_scene_with_quality(desktop_high());
    scene.width = size.0;
    scene.height = size.1;
    scene.eye[0] = if n.is_multiple_of(2) { 0.0 } else { 0.05 };
    let (texture, view) = outputs
        .targets
        .get(&size)
        .unwrap_or_else(|| panic!("no output at {size:?}"));
    let target = FrameTarget {
        color: Some(texture),
        ..FrameTarget::full(view, FORMAT, size.0, size.1)
    };
    renderer
        .render_into(device, &scene, target)
        .unwrap_or_else(|err| panic!("frame at {size:?}: {err}"));
}

fn settled_report(device: &RenderDevice) -> MemoryReport {
    device.wait_idle().unwrap_or_else(|err| panic!("wait: {err}"));
    // A second poll after the wait frees what the first one's callbacks released.
    device.wait_idle().unwrap_or_else(|err| panic!("wait: {err}"));
    regia_render::memory_report(device)
}

/// Labels whose live bytes grew from `before` to `after`, largest first.
fn growth(before: &MemoryReport, after: &MemoryReport) -> Vec<(String, i64, i64)> {
    let old: HashMap<&str, (u64, u32)> = before
        .labels
        .iter()
        .map(|l| (l.label.as_str(), (l.bytes, l.count)))
        .collect();
    let mut grown: Vec<(String, i64, i64)> = after
        .labels
        .iter()
        .filter_map(|l| {
            let (bytes, count) = old.get(l.label.as_str()).copied().unwrap_or((0, 0));
            let db = l.bytes as i64 - bytes as i64;
            (db > 0).then(|| (l.label.clone(), db, i64::from(l.count) - i64::from(count)))
        })
        .collect();
    grown.sort_by_key(|g| std::cmp::Reverse(g.1));
    grown
}

#[test]
fn resizing_the_main_view_returns_its_memory() {
    let Some(device) = device() else { return };
    let cycles: u32 = regia_core::brand::env_var("REGIA_RESIZE_CYCLES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let drag: Vec<(u32, u32)> = (0..16).map(|i| (BASE.0 + 24 * i, BASE.1 + 13 * i)).collect();
    let mut sizes = vec![BASE, SMALL, LARGE];
    sizes.extend(&drag);
    let outputs = Outputs::new(&device, &sizes);
    let lab = scene();
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        time: Some(TimeOfDay::noon()),
        ..OutdoorParams::default()
    });

    let mut n = 0u32;
    let mut run = |renderer: &mut FrameRenderer, size: (u32, u32), frames: u32| {
        for _ in 0..frames {
            frame(renderer, &device, &lab, &outputs, size, n);
            n += 1;
        }
    };
    // Warm at the base size: pipelines, IBL, atmosphere tables, history.
    run(&mut renderer, BASE, 24);
    let base = settled_report(&device);
    let base_textures = renderer.texture_memory().resident_bytes;
    eprintln!(
        "baseline: available {} allocated {} MB reserved {} MB",
        base.available,
        base.allocated_bytes >> 20,
        base.reserved_bytes >> 20
    );

    // Alternate the output size (a texture view's old flip, a fullscreen toggle).
    for _ in 0..cycles {
        run(&mut renderer, LARGE, 2);
        run(&mut renderer, SMALL, 2);
    }
    let peak_alternate = regia_render::memory_report(&device);
    // Drag the window through a run of sizes.
    for &size in &drag {
        run(&mut renderer, size, 2);
    }
    let peak_drag = regia_render::memory_report(&device);
    // Back at the base size; let the renderer settle.
    run(&mut renderer, BASE, 24);
    let after = settled_report(&device);
    let after_textures = renderer.texture_memory().resident_bytes;
    eprintln!(
        "after alternate {} MB, after drag {} MB, back at base {} MB (reserved {} MB)",
        peak_alternate.allocated_bytes >> 20,
        peak_drag.allocated_bytes >> 20,
        after.allocated_bytes >> 20,
        after.reserved_bytes >> 20
    );
    let grown = growth(&base, &after);
    for (label, bytes, count) in grown.iter().take(24) {
        eprintln!("  grew {label:?}: {:+} KB, {count:+} allocations", bytes / 1024);
    }

    assert_eq!(
        after_textures, base_textures,
        "the renderer's texture report changed across resizes"
    );
    if !base.available {
        eprintln!("skip allocator check: the backend has no allocator report");
        return;
    }
    eprintln!(
        "wgpu validation pool: {} KB at baseline, {} KB after (not compared)",
        validation_pool_bytes(&base) / 1024,
        validation_pool_bytes(&after) / 1024
    );
    let (base_bytes, after_bytes) = (allocated_without_pool(&base), allocated_without_pool(&after));
    assert!(
        after_bytes <= base_bytes + MARGIN_BYTES,
        "resizing kept {} MB more GPU memory than the baseline ({} → {} MB); grew: {:?}",
        after_bytes.saturating_sub(base_bytes) >> 20,
        base_bytes >> 20,
        after_bytes >> 20,
        &grown[..grown.len().min(12)]
    );
}
