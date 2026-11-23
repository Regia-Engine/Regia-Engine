// SPDX-License-Identifier: MPL-2.0
//! Unique pipelines per shipped profile, pinned (ADR-327).
//!
//! DX12 has no engine pipeline cache (`backend_policy::uses_pipeline_cache` is Vulkan only), so on
//! a cold Windows DX12 start every pipeline a profile warms is a driver compile: the count is the
//! cost. This runs install prepare (`prepare::run`: the warm-unit list, i.e. the splash, the
//! canonical lit frame in both window formats and the profile's frame in both, plus the deferred
//! variants) once per shipped profile, each on a fresh device, and pins how many pipelines each
//! creates. A permutation explosion (a new shader define multiplying the variants) fails here
//! instead of on a tester's first launch.
//!
//! The pins are counted on the CPU reference adapter (lavapipe, the device goldens are blessed
//! on): an adapter's fallbacks (storage limits, indirect-count support) change which variants
//! exist, so another adapter only has to stay under twice its pin. One warm worker on the reference
//! adapter, so the order of builds is fixed. One test in its own binary: the pipeline counter is
//! process-wide.
//!
//! Re-pinning is like re-blessing a golden: only for an intended change, with the reason in the
//! commit message. The failure message prints the table to paste.

use std::time::Duration;

use regia_render::RenderProfilePreset;
use regia_render::prepare::{self, PrepareRequest};

/// Pipelines each profile's prepare creates on the CPU reference adapter. `0` is not pinned yet:
/// the test fails on the reference adapter and prints the measured value.
///
/// DesktopHigh and DesktopUltra 160 -> 164 (lane v06-gate-reds-render): the DDGI runtime builds
/// ten compute pipelines, not eight, since ADR-7870's young-probe budget (lane gi-young-budget,
/// 0fe2944ba: `ddgi young histogram` and `ddgi young select`, `ddgi_gpu.rs`), and the two
/// profiles with dynamic GI warm two DDGI runtimes (16 -> 20 at the `ddgi_gpu.rs` site, counted by
/// `pipeline_warm::warm_site_costs` at v0.5.0 and at a03439ea2 on the build server lavapipe).
const PINNED: [(RenderProfilePreset, u64); 6] = [
    (RenderProfilePreset::MobileLow, 77),
    (RenderProfilePreset::MobileHigh, 85),
    (RenderProfilePreset::DesktopLow, 111),
    (RenderProfilePreset::DesktopHigh, 164),
    (RenderProfilePreset::DesktopUltra, 164),
    (RenderProfilePreset::Deck, 120),
];

/// How far a hardware adapter may exceed the reference pin before it counts as an explosion.
const HARDWARE_FACTOR: u64 = 2;

#[test]
fn pipelines_per_shipped_profile_stay_pinned() {
    // The cache variables would send the warm to another directory. Nothing else in this binary
    // reads the environment concurrently.
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
    assert_eq!(
        PINNED.map(|(preset, _)| preset),
        RenderProfilePreset::ALL,
        "every shipped profile is pinned, in table order"
    );
    // The reference run sees only software adapters (the lavapipe pass); without opening an extra
    // device, which some drivers dislike several of per process.
    let adapters = regia_render::list_adapters();
    assert!(
        !adapters.is_empty(),
        "no GPU adapter (install Mesa lavapipe or use WARP)"
    );
    let reference = adapters.iter().all(|a| a.device_type == wgpu::DeviceType::Cpu);
    // One directory for every run: the Vulkan cache from one run speeds up the next one's
    // compiles, and a cache hit still counts as a created pipeline.
    let dir = std::env::temp_dir().join(format!("regia-pipeline-count-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");

    let mut measured = Vec::new();
    for (preset, _) in PINNED {
        let mut request = PrepareRequest::new(dir.clone());
        request.profile = Some(preset);
        // One worker on the reference adapter fixes the build order; hardware keeps the Boot budget
        // so the run stays well inside the GPU pass's per-binary limit.
        request.threads = reference.then_some(1);
        request.nice = false;
        request.skip_if_current = false;
        request.timeout = Duration::from_secs(600);
        let before = regia_render::pipeline_cache::pipelines_created();
        let report = prepare::run(&request, |_| {})
            .unwrap_or_else(|err| panic!("prepare for {} failed: {err}", preset.name()));
        assert!(!report.skipped, "{}: a forced run warms", preset.name());
        assert!(
            report.units.contains(&format!("profile:{}:", preset.name())),
            "{}: the profile's frame is in the unit list ({})",
            preset.name(),
            report.units
        );
        let created = report.pipelines.saturating_sub(before);
        assert!(created > 0, "{}: the warm created no pipeline", preset.name());
        measured.push((preset, created));
    }
    let _ = std::fs::remove_dir_all(&dir);

    let table: Vec<String> = measured
        .iter()
        .map(|(preset, n)| format!("    (RenderProfilePreset::{}, {n}),", preset.name()))
        .collect();
    println!(
        "pipelines per profile ({} adapter):\n{}",
        if reference { "reference" } else { "hardware" },
        table.join("\n")
    );
    let mut problems = Vec::new();
    for ((preset, pin), (_, created)) in PINNED.iter().zip(&measured) {
        if reference {
            if *pin == 0 {
                problems.push(format!("{}: not pinned yet (measured {created})", preset.name()));
            } else if created != pin {
                problems.push(format!("{}: {created} pipelines, pinned {pin}", preset.name()));
            }
        } else if *pin > 0 && *created > pin * HARDWARE_FACTOR {
            problems.push(format!(
                "{}: {created} pipelines, over {HARDWARE_FACTOR}x the reference pin {pin}",
                preset.name()
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "{}\nIf the change is intended, set PINNED to:\n{}",
        problems.join("\n"),
        table.join("\n")
    );
}
