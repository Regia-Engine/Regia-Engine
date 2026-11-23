// SPDX-License-Identifier: MPL-2.0
//! Shipped pipeline caches (`pipeline_cache` module docs, book: Shipped pipeline caches): a
//! package's cache for this exact adapter, driver and shader set seeds an empty local cache, so
//! the warm-up compiles almost nothing; a wrong one (another driver's key, garbage, a payload whose
//! wgpu or driver header does not match) is rejected without a crash and the warm compiles as
//! before, drawing the same pixels. One test in its own binary: the cache install is process-wide.

use std::path::{Path, PathBuf};
use std::time::Instant;

use regia_render::pipeline_cache::{self, CacheLoad, ShippedLoad};
use regia_render::pipeline_warm::canonical_frame;
use regia_render::{FrameRenderer, RenderDevice};

struct Run {
    warm_ms: f32,
    pixels: Vec<u8>,
    load: Option<CacheLoad>,
    shipped: Option<ShippedLoad>,
    key: pipeline_cache::PipelineCacheKey,
    cpu: bool,
}

/// A fresh device whose cache lives in `local` and reads shipped caches from `shipped`; warms the
/// canonical frame, renders it once, saves the cache.
fn run(local: &Path, shipped: &Path) -> Option<Run> {
    pipeline_cache::set_directory(Some(local.to_path_buf()));
    pipeline_cache::set_shipped_directory(Some(shipped.to_path_buf()));
    let device = RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)");
    if !device.device.features().contains(wgpu::Features::PIPELINE_CACHE) {
        eprintln!("adapter has no PIPELINE_CACHE; shipped caches do not apply");
        return None;
    }
    let installed = pipeline_cache::installed().expect("a cache directory installs a cache");
    let key = installed.key().clone();
    let status = pipeline_cache::status();
    drop(installed);
    let frame = canonical_frame();
    let (view_index, _) = frame.window_views()[0];
    let scene = frame.frame_scene(view_index, 96, 64);
    let mut renderer = FrameRenderer::new();
    let start = Instant::now();
    renderer
        .warm_extracted(&device, &frame, wgpu::TextureFormat::Rgba8Unorm, true)
        .expect("warm");
    let warm_ms = start.elapsed().as_secs_f32() * 1000.0;
    let pixels = renderer.render_offscreen(&device, &scene).expect("frame").pixels;
    pipeline_cache::save_installed()
        .expect("installed")
        .expect("save");
    let cpu = device.adapter.get_info().device_type == wgpu::DeviceType::Cpu;
    drop(renderer);
    drop(device);
    assert!(
        pipeline_cache::installed().is_none(),
        "the device's drop ends the install"
    );
    Some(Run {
        warm_ms,
        pixels,
        load: status.load,
        shipped: status.shipped,
        key,
        cpu,
    })
}

fn fresh(root: &Path, name: &str) -> PathBuf {
    let dir = root.join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

#[test]
fn shipped_cache_seeds_and_wrong_caches_are_rejected_safely() {
    // The drivers' own disk shader caches would hide the cold compile (a tester's first launch
    // has them empty too). Set before any driver is loaded; nothing else in this binary reads
    // the environment concurrently.
    #[allow(unsafe_code)]
    // SAFETY: single test in its own binary, before any other thread or driver exists.
    unsafe {
        std::env::set_var("__GL_SHADER_DISK_CACHE", "0");
        std::env::set_var("MESA_SHADER_CACHE_DISABLE", "true");
    }
    // A directory from the environment would win over the ones this test sets, and its file would
    // be found where the test needs an empty local cache.
    #[allow(unsafe_code)]
    // SAFETY: one test, before any warm worker or driver exists.
    unsafe {
        std::env::remove_var("REGIA_PIPELINE_CACHE_DIR");
        std::env::remove_var("FERRITE_PIPELINE_CACHE_DIR");
        std::env::remove_var("REGIA_PIPELINE_CACHE");
        std::env::remove_var("FERRITE_PIPELINE_CACHE");
        std::env::remove_var("REGIA_SHIPPED_PIPELINE_CACHE_DIR");
        std::env::remove_var("FERRITE_SHIPPED_PIPELINE_CACHE_DIR");
    }
    let root = std::env::temp_dir().join(format!("regia-shipped-fpc-{}", std::process::id()));
    let none = fresh(&root, "no-ship");

    // The build machine: a cold warm fills its local cache.
    let build = fresh(&root, "build");
    let Some(cold) = run(&build, &none) else {
        return;
    };
    assert_eq!(cold.load, Some(CacheLoad::Missing));
    assert_eq!(cold.shipped, Some(ShippedLoad::NoMatch));
    let built = std::fs::read(build.join(cold.key.file_name())).expect("the warm saved a cache");
    let payload = pipeline_cache::decode(&built, &cold.key).expect("valid").to_vec();

    // The package ships it under its key's name; a tester's first launch is seeded from it.
    let ship = fresh(&root, "ship");
    std::fs::write(ship.join(cold.key.shipped_file_name()), &built).expect("ship");
    let tester = fresh(&root, "tester");
    let seeded = run(&tester, &ship).expect("same adapter");
    assert_eq!(seeded.load, Some(CacheLoad::Seeded(payload.len() as u64)));
    assert!(matches!(seeded.shipped, Some(ShippedLoad::Seeded(_))));
    assert_eq!(seeded.pixels, cold.pixels, "a seeded cache draws the same frame");
    assert!(
        tester.join(cold.key.file_name()).is_file(),
        "the first save writes the local cache"
    );
    // The next launch uses the local file and ignores the shipped one.
    let again = run(&tester, &ship).expect("same adapter");
    assert!(matches!(again.load, Some(CacheLoad::Loaded(_))));
    assert_eq!(again.shipped, None);

    // Wrong caches, each under this adapter's shipped name. None may crash or change pixels.
    let mut other_driver = cold.key.clone();
    other_driver.driver_info.push_str("-other");
    let mut wgpu_header = payload.clone();
    for b in wgpu_header.iter_mut().skip(8).take(40) {
        *b ^= 0x5a;
    }
    // Past wgpu's 64-byte header: the driver's own header (Vulkan: length, version, vendor,
    // device, pipeline-cache UUID). wgpu passes it through; the driver must reject it.
    let mut driver_header = payload.clone();
    for b in driver_header.iter_mut().skip(64 + 8).take(24) {
        *b ^= 0xa5;
    }
    let garbage: Vec<u8> = (0..payload.len().max(256))
        .map(|i| (i * 131 % 251) as u8)
        .collect();
    let cases: [(&str, Vec<u8>, bool); 4] = [
        (
            "another driver's key",
            pipeline_cache::encode(&other_driver, &payload),
            false,
        ),
        (
            "garbage payload",
            pipeline_cache::encode(&cold.key, &garbage),
            true,
        ),
        (
            "wgpu header altered",
            pipeline_cache::encode(&cold.key, &wgpu_header),
            true,
        ),
        (
            "driver header altered",
            pipeline_cache::encode(&cold.key, &driver_header),
            true,
        ),
    ];
    for (name, file, reaches_driver) in cases {
        let ship = fresh(&root, "ship-wrong");
        std::fs::write(ship.join(cold.key.shipped_file_name()), &file).expect("ship");
        let tester = fresh(&root, "tester-wrong");
        let wrong = run(&tester, &ship).expect("same adapter");
        if reaches_driver {
            // Passes Regia Engine's checks (right key, intact checksum): wgpu and the driver validate
            // their own headers and fall back to an empty cache.
            assert!(matches!(wrong.load, Some(CacheLoad::Seeded(_))), "{name}");
        } else {
            assert_eq!(wrong.load, Some(CacheLoad::Missing), "{name}");
            assert!(
                matches!(
                    wrong.shipped,
                    Some(ShippedLoad::Discarded(
                        _,
                        pipeline_cache::DiscardReason::KeyMismatch
                    ))
                ),
                "{name}"
            );
        }
        assert_eq!(wrong.pixels, cold.pixels, "{name}: same frame");
        println!("{name}: warm {:.0} ms", wrong.warm_ms);
    }

    println!(
        "canonical frame warm: cold {:.0} ms, seeded {:.0} ms, local {:.0} ms",
        cold.warm_ms, seeded.warm_ms, again.warm_ms
    );
    if !cold.cpu {
        assert!(
            seeded.warm_ms < cold.warm_ms * 0.5,
            "a seeded warm compiles almost nothing: cold {:.0} ms, seeded {:.0} ms",
            cold.warm_ms,
            seeded.warm_ms
        );
    }
    pipeline_cache::set_directory(None);
    pipeline_cache::set_shipped_directory(None);
    let _ = std::fs::remove_dir_all(&root);
}
