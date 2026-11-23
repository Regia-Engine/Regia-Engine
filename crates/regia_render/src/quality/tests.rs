// SPDX-License-Identifier: MPL-2.0
use std::time::{Duration, Instant};

use super::*;

const NVIDIA: u32 = 0x10de;
const AMD: u32 = 0x1002;
const INTEL: u32 = 0x8086;
const QUALCOMM: u32 = 0x5143;
const ARM: u32 = 0x13b5;

fn table() -> GpuTierTable {
    match GpuTierTable::from_ron(BUILTIN_GPU_TIERS) {
        Ok(table) => table,
        Err(err) => panic!("built-in gpu_tiers.ron must parse: {err}"),
    }
}

fn tier_of(name: &str, vendor: u32, vram_mb: Option<u64>) -> QualityTier {
    detect(&AdapterProfile::named(name, vendor, vram_mb), &table()).tier
}

#[test]
fn builtin_table_parses_with_unique_ids_and_evidence() {
    let table = table();
    assert_eq!(table.version, 1);
    assert_eq!(GpuTierTable::builtin(), table);
    let mut ids: Vec<&str> = table.rules.iter().map(|r| r.id.as_str()).collect();
    let count = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), count, "row ids must be unique");
    for rule in &table.rules {
        assert!(
            !rule.evidence.note().is_empty(),
            "{} has no evidence note",
            rule.id
        );
    }
    // The measured rows are the GPUs Regia Engine was benchmarked on.
    let measured: Vec<&str> = table
        .rules
        .iter()
        .filter(|r| matches!(r.evidence, Evidence::Measured(_)))
        .map(|r| r.id.as_str())
        .collect();
    assert_eq!(
        measured,
        [
            "steam_deck",
            "nvidia_rtx_5090_5080",
            "nvidia_rtx_3080_3090",
            "nvidia_gtx_1080_1070",
            "intel_n_series",
            "intel_arc_bmg_g31"
        ]
    );
}

#[test]
fn measured_gpus_map_as_benchmarked() {
    assert_eq!(
        tier_of("NVIDIA GeForce RTX 5090", NVIDIA, Some(32_607)),
        QualityTier::Ultra
    );
    assert_eq!(
        tier_of("NVIDIA GeForce RTX 5080", NVIDIA, Some(16_303)),
        QualityTier::Ultra
    );
    assert_eq!(
        tier_of("NVIDIA GeForce RTX 3080 Ti", NVIDIA, Some(12_288)),
        QualityTier::High
    );
    assert_eq!(
        tier_of("NVIDIA GeForce GTX 1080", NVIDIA, Some(8_192)),
        QualityTier::Low
    );
    let d = detect(
        &AdapterProfile::named("NVIDIA GeForce RTX 3080 Ti", NVIDIA, Some(12_288)),
        &table(),
    );
    assert!(
        d.reason.starts_with("NVIDIA GeForce RTX 3080 Ti, 12 GB → High"),
        "{}",
        d.reason
    );
    assert_eq!(d.evidence, "measured");
    assert_eq!(d.rule, "nvidia_rtx_3080_3090");
    assert!(d.caps.is_empty());
}

/// The farm's real adapters, as their drivers report them (vulkaninfo / `live_adapter_detection`
/// on 2026-10-09): name, PCI ids and the device-local heap.
#[test]
fn the_farm_boxes_detect_as_measured() {
    let real = |name: &str, device: u32, vram_mb: u64| {
        let mut a = AdapterProfile::named(name, NVIDIA, Some(vram_mb));
        a.device = device;
        detect(&a, &table()).reason
    };
    assert_eq!(
        real("NVIDIA GeForce GTX 1080", 0x1b80, 8_192),
        "NVIDIA GeForce GTX 1080, 8 GB → Low (nvidia_gtx_1080_1070, measured)"
    );
    assert_eq!(
        real("NVIDIA GeForce RTX 3080 Ti", 0x2208, 12_288),
        "NVIDIA GeForce RTX 3080 Ti, 12 GB → High (nvidia_rtx_3080_3090, measured)"
    );
    assert_eq!(
        real("NVIDIA GeForce RTX 5080", 0x2c02, 16_303),
        "NVIDIA GeForce RTX 5080, 16 GB → Ultra (nvidia_rtx_5090_5080, measured)"
    );
}

/// The Steam Hardware & Software Survey, September 2026, top 40 video cards (by share): every one
/// matches a table row (none falls back) with the tier the RON coverage comment lists. All are
/// table-by-class except the measured RTX 5080 and RTX 3080 rows.
#[test]
fn steam_survey_top_40_is_covered() {
    use QualityTier::{High, Low, Medium, Ultra};
    let i = DeviceClass::Integrated;
    let d = DeviceClass::Discrete;
    let top: [(&str, u32, DeviceClass, QualityTier); 40] = [
        ("NVIDIA GeForce RTX 5070", NVIDIA, d, High),
        ("NVIDIA GeForce RTX 5060", NVIDIA, d, Medium),
        ("NVIDIA GeForce RTX 5060 Ti", NVIDIA, d, Medium),
        ("NVIDIA GeForce RTX 4060", NVIDIA, d, Medium),
        ("NVIDIA GeForce RTX 3060", NVIDIA, d, Medium),
        ("NVIDIA GeForce RTX 4060 Laptop GPU", NVIDIA, d, Medium),
        ("NVIDIA GeForce RTX 3050", NVIDIA, d, Low),
        ("NVIDIA GeForce RTX 4060 Ti", NVIDIA, d, Medium),
        ("NVIDIA GeForce RTX 5060 Laptop GPU", NVIDIA, d, Medium),
        ("NVIDIA GeForce GTX 1650", NVIDIA, d, Low),
        ("NVIDIA GeForce RTX 4070", NVIDIA, d, High),
        ("NVIDIA GeForce RTX 3060 Ti", NVIDIA, d, Medium),
        ("NVIDIA GeForce RTX 5080", NVIDIA, d, Ultra),
        ("NVIDIA GeForce RTX 3070", NVIDIA, d, Medium),
        ("NVIDIA GeForce RTX 5070 Ti", NVIDIA, d, Ultra),
        ("NVIDIA GeForce RTX 4070 SUPER", NVIDIA, d, High),
        ("NVIDIA GeForce RTX 3060 Laptop GPU", NVIDIA, d, Low),
        ("NVIDIA GeForce RTX 4050 Laptop GPU", NVIDIA, d, Low),
        ("NVIDIA GeForce RTX 2060", NVIDIA, d, Low),
        ("Intel(R) Iris(R) Xe Graphics", INTEL, i, Low),
        ("AMD Radeon Graphics", AMD, i, Low),
        ("AMD Radeon RX 9070 XT", AMD, d, Ultra),
        ("NVIDIA GeForce GTX 1060 6GB", NVIDIA, d, Low),
        ("NVIDIA GeForce RTX 3080", NVIDIA, d, High),
        ("NVIDIA GeForce GTX 1660 SUPER", NVIDIA, d, Low),
        ("Intel(R) UHD Graphics", INTEL, i, Low),
        ("NVIDIA GeForce GTX 1050 Ti", NVIDIA, d, Low),
        ("NVIDIA GeForce RTX 4070 Ti", NVIDIA, d, High),
        ("NVIDIA GeForce RTX 3070 Ti", NVIDIA, d, Medium),
        ("NVIDIA GeForce RTX 4070 Laptop GPU", NVIDIA, d, Medium),
        ("AMD Radeon(TM) Graphics", AMD, i, Low),
        ("NVIDIA GeForce GTX 1660 Ti", NVIDIA, d, Low),
        ("AMD Radeon RX 9060 XT", AMD, d, Medium),
        ("AMD Radeon RX 6600", AMD, d, Low),
        ("NVIDIA GeForce RTX 2060 SUPER", NVIDIA, d, Low),
        ("NVIDIA GeForce RTX 4070 Ti SUPER", NVIDIA, d, High),
        ("NVIDIA GeForce RTX 4090", NVIDIA, d, Ultra),
        ("NVIDIA GeForce RTX 3050 6GB Laptop GPU", NVIDIA, d, Low),
        ("NVIDIA GeForce RTX 5070 Laptop GPU", NVIDIA, d, Medium),
        ("NVIDIA GeForce RTX 4080 SUPER", NVIDIA, d, Ultra),
    ];
    let table = table();
    for (name, vendor, class, want) in top {
        // VRAM unknown: the row alone decides (no VRAM floor applies).
        let mut adapter = AdapterProfile::named(name, vendor, None);
        adapter.class = class;
        let det = detect(&adapter, &table);
        assert_ne!(det.rule, "fallback", "{name} must match a row");
        assert_eq!(det.tier, want, "{name}: {}", det.reason);
    }
}

#[test]
fn known_gpu_classes_map_as_intended() {
    let cases: &[(&str, u32, Option<u64>, QualityTier)] = &[
        (
            "NVIDIA GeForce RTX 4090",
            NVIDIA,
            Some(24_564),
            QualityTier::Ultra,
        ),
        (
            "NVIDIA GeForce RTX 5070 Ti",
            NVIDIA,
            Some(16_303),
            QualityTier::Ultra,
        ),
        (
            "NVIDIA GeForce RTX 4070 SUPER",
            NVIDIA,
            Some(12_282),
            QualityTier::High,
        ),
        ("NVIDIA GeForce RTX 3080", NVIDIA, Some(10_240), QualityTier::High),
        (
            "NVIDIA GeForce RTX 3070",
            NVIDIA,
            Some(8_192),
            QualityTier::Medium,
        ),
        (
            "NVIDIA GeForce RTX 4060 Ti",
            NVIDIA,
            Some(8_188),
            QualityTier::Medium,
        ),
        (
            "NVIDIA GeForce RTX 3060",
            NVIDIA,
            Some(12_288),
            QualityTier::Medium,
        ),
        (
            "NVIDIA GeForce RTX 2080 SUPER",
            NVIDIA,
            Some(8_192),
            QualityTier::Medium,
        ),
        ("NVIDIA GeForce RTX 2060", NVIDIA, Some(6_144), QualityTier::Low),
        (
            "NVIDIA GeForce GTX 1660 SUPER",
            NVIDIA,
            Some(6_144),
            QualityTier::Low,
        ),
        (
            "NVIDIA GeForce GTX 1080 Ti",
            NVIDIA,
            Some(11_264),
            QualityTier::Low,
        ),
        (
            "NVIDIA GeForce RTX 4090 Laptop GPU",
            NVIDIA,
            Some(16_376),
            QualityTier::High,
        ),
        (
            "NVIDIA GeForce RTX 4060 Laptop GPU",
            NVIDIA,
            Some(8_188),
            QualityTier::Medium,
        ),
        (
            "NVIDIA GeForce RTX 3050 Laptop GPU",
            NVIDIA,
            Some(4_096),
            QualityTier::Low,
        ),
        (
            "AMD Radeon RX 7900 XTX (RADV NAVI31)",
            AMD,
            Some(24_560),
            QualityTier::Ultra,
        ),
        ("AMD Radeon RX 9070 XT", AMD, Some(16_304), QualityTier::Ultra),
        ("AMD Radeon RX 7900 GRE", AMD, Some(16_304), QualityTier::High),
        (
            "AMD Radeon RX 6800 XT (RADV NAVI21)",
            AMD,
            Some(16_368),
            QualityTier::High,
        ),
        ("AMD Radeon RX 6700 XT", AMD, Some(12_272), QualityTier::Medium),
        ("AMD Radeon RX 6600 XT", AMD, Some(8_176), QualityTier::Medium),
        ("AMD Radeon RX 6600", AMD, Some(8_176), QualityTier::Low),
        ("AMD Radeon RX 580 Series", AMD, Some(8_192), QualityTier::Low),
        (
            "Intel(R) Arc(TM) B580 Graphics",
            INTEL,
            Some(12_216),
            QualityTier::Low,
        ),
        (
            "Intel(R) Arc(TM) A380 Graphics",
            INTEL,
            Some(6_000),
            QualityTier::Low,
        ),
    ];
    let table = table();
    for (name, vendor, vram, want) in cases {
        let d = detect(&AdapterProfile::named(name, *vendor, *vram), &table);
        assert_eq!(d.tier, *want, "{name}: {}", d.reason);
        assert_ne!(d.rule, "fallback", "{name} should match a row");
    }
}

#[test]
fn steam_deck_integrated_and_software_gpus() {
    let table = table();
    let mut deck = AdapterProfile::named("AMD Custom GPU 0405 (RADV VANGOGH)", AMD, Some(1_024));
    deck.class = DeviceClass::Integrated;
    let d = detect(&deck, &table);
    assert_eq!(d.tier, QualityTier::Deck, "{}", d.reason);
    // By device id alone (an OLED Deck whose name the table does not list).
    let mut oled = AdapterProfile::named("AMD Radeon Graphics", AMD, Some(4_096));
    oled.class = DeviceClass::Integrated;
    oled.device = 0x1435;
    assert_eq!(detect(&oled, &table).tier, QualityTier::Deck);

    let mut igpu = AdapterProfile::named("AMD Radeon Graphics (RADV RAPHAEL_MENDOCINO)", AMD, Some(2_048));
    igpu.class = DeviceClass::Integrated;
    let d = detect(&igpu, &table);
    assert_eq!(d.tier, QualityTier::Low);
    assert_eq!(d.rule, "amd_integrated");
    assert!(d.reason.contains("integrated → Low"), "{}", d.reason);

    let mut vega = AdapterProfile::named("AMD Radeon Vega 8 Graphics", AMD, Some(2_048));
    vega.class = DeviceClass::Integrated;
    assert_eq!(
        detect(&vega, &table).rule,
        "amd_integrated",
        "integrated Vega is not the discrete row"
    );
    // An integrated GPU of another vendor still falls back to Low.
    let mut other = AdapterProfile::named("Moore Threads iGPU", 0x1ed5, Some(1_024));
    other.class = DeviceClass::Integrated;
    let d = detect(&other, &table);
    assert_eq!((d.tier, d.rule.as_str()), (QualityTier::Low, "fallback"));

    let mut lavapipe = AdapterProfile::named("llvmpipe (LLVM 19.1.7, 256 bits)", 0x10005, Some(64_000));
    lavapipe.class = DeviceClass::Cpu;
    assert_eq!(detect(&lavapipe, &table).tier, QualityTier::Low);
}

#[test]
fn android_gpus_get_mobile_tiers() {
    let table = table();
    let phone = |name: &str, vendor: u32| {
        let mut a = AdapterProfile::named(name, vendor, None);
        a.class = DeviceClass::Integrated;
        a.android = true;
        a.block_compression = false;
        detect(&a, &table).tier
    };
    assert_eq!(phone("Mali-G78", ARM), QualityTier::MobileHigh);
    assert_eq!(phone("Adreno (TM) 740", QUALCOMM), QualityTier::MobileHigh);
    assert_eq!(phone("Adreno (TM) 610", QUALCOMM), QualityTier::MobileLow);
    assert_eq!(phone("Mali-G52 MC2", ARM), QualityTier::MobileLow);
    assert_eq!(phone("Some Future GPU", 0x1234), QualityTier::MobileHigh);
    // A desktop row on Android still lands on a mobile tier.
    assert_eq!(phone("NVIDIA GeForce RTX 5090", NVIDIA), QualityTier::MobileHigh);
}

#[test]
fn unknown_gpus_fall_back_by_vram_and_limits() {
    let table = table();
    let unknown = |vram: Option<u64>| {
        detect(
            &AdapterProfile::named("Moore Threads MTT S80", 0x1ed5, vram),
            &table,
        )
    };
    // ADR-173: with no first-launch probe the fallback is one tier below its VRAM guess.
    let big = unknown(Some(24_000));
    assert_eq!(
        big.tier,
        QualityTier::Medium,
        "the fallback never picks Ultra or High"
    );
    assert_eq!(big.rule, "fallback");
    assert_eq!(big.evidence, "fallback");
    assert!(big.reason.contains("fallback"), "{}", big.reason);
    assert_eq!(unknown(Some(16_384)).tier, QualityTier::Medium);
    assert_eq!(unknown(Some(8_192)).tier, QualityTier::Low);
    assert_eq!(unknown(Some(4_096)).tier, QualityTier::Low);
    assert_eq!(unknown(None).tier, QualityTier::Low);
    // An unknown integrated GPU is Low whatever memory it reports.
    let mut igpu = AdapterProfile::named("Unknown Vendor iGPU", 0x1ed5, Some(32_000));
    igpu.class = DeviceClass::Integrated;
    assert_eq!(detect(&igpu, &table).tier, QualityTier::Low);

    // A GPU below the engine's limit floor is capped at Low, whatever its row says.
    let mut weak = AdapterProfile::named("NVIDIA GeForce RTX 5090", NVIDIA, Some(32_607));
    weak.max_bind_groups = 4;
    let d = detect(&weak, &table);
    assert_eq!(d.tier, QualityTier::Low);
    assert!(d.caps[0].contains("4 bind groups"), "{:?}", d.caps);
    let mut no_bc = AdapterProfile::named("Unknown", 0x1234, Some(16_000));
    no_bc.block_compression = false;
    assert_eq!(detect(&no_bc, &table).tier, QualityTier::Low);
}

#[test]
fn vram_floors_step_a_row_down() {
    // An RTX 4080 that reports 6 GB (a misreport or a cut-down part): Ultra → High → Medium.
    let d = detect(
        &AdapterProfile::named("NVIDIA GeForce RTX 4080", NVIDIA, Some(6_000)),
        &table(),
    );
    assert_eq!(d.tier, QualityTier::Medium, "{}", d.reason);
    assert_eq!(d.caps.len(), 2, "{:?}", d.caps);
    // The Deck and Low have no floor.
    let mut deck = AdapterProfile::named("AMD Custom GPU 0405", AMD, Some(512));
    deck.class = DeviceClass::Integrated;
    assert_eq!(detect(&deck, &table()).tier, QualityTier::Deck);
}

#[test]
fn prepended_rows_win() {
    let mut table = table();
    table.prepend([GpuRule {
        id: "game_override".to_string(),
        vendor: Some(NVIDIA),
        device_ids: Vec::new(),
        any: vec!["gtx 1080".to_string()],
        all: Vec::new(),
        none: Vec::new(),
        class: None,
        tier: QualityTier::Medium,
        evidence: Evidence::Guess("test".to_string()),
    }]);
    let d = detect(
        &AdapterProfile::named("NVIDIA GeForce GTX 1080", NVIDIA, Some(8_192)),
        &table,
    );
    assert_eq!((d.tier, d.rule.as_str()), (QualityTier::Medium, "game_override"));
}

#[test]
fn tiers_map_onto_presets() {
    assert_eq!(QualityTier::Medium.preset(), RenderProfilePreset::DesktopHigh);
    let medium = QualityTier::Medium.profile();
    assert_eq!(medium.render_scale, QualityTier::MEDIUM_RENDER_SCALE);
    assert!(!medium.gi_probes);
    assert_eq!(QualityTier::Medium.default_gi_look(), GiLook::Balanced);
    assert_eq!(
        QualityTier::High.profile(),
        RenderProfile::from_preset(RenderProfilePreset::DesktopHigh)
    );
    for tier in QualityTier::ALL {
        let mut settings = RenderSettings {
            gi_look: Some(GiLook::Balanced),
            ..RenderSettings::default()
        };
        tier.apply(&mut settings);
        assert_eq!(QualityTier::of_settings(&settings), Some(tier), "{tier}");
        assert_eq!(
            settings.gi_look,
            Some(GiLook::Balanced),
            "the player's look is kept"
        );
        assert_eq!(QualityTier::from_name(tier.name()), Some(tier));
    }
    assert_eq!(
        QualityTier::from_name("mobile_high"),
        Some(QualityTier::MobileHigh)
    );
    for preset in RenderProfilePreset::ALL {
        assert_eq!(QualityTier::of_preset(preset).preset(), preset);
    }
    let mut down = vec![QualityTier::Ultra];
    while let Some(next) = down.last().and_then(|t| t.step_down()) {
        down.push(next);
    }
    assert_eq!(
        down,
        [
            QualityTier::Ultra,
            QualityTier::High,
            QualityTier::Medium,
            QualityTier::Low,
            QualityTier::VeryLow
        ]
    );
    assert_eq!(QualityTier::Deck.step_down(), None);
    assert_eq!(QualityTier::Low.for_platform(true), QualityTier::MobileLow);
    assert_eq!(QualityTier::MobileHigh.for_platform(false), QualityTier::Low);
}

fn result(frames: u32, p95_ms: f32) -> ProbeResult {
    ProbeResult {
        frames,
        p50_ms: p95_ms * 0.8,
        p95_ms,
        source: ProbeSource::Gpu,
        width: 2560,
        height: 1440,
        timed_out: frames == 0,
        aborted: false,
        gpu_busy: None,
    }
}

#[test]
fn probe_verdict_steps_down_once_when_over_budget() {
    let v = judge_probe(QualityTier::High, &result(300, 21.9), 60.0);
    assert_eq!((v.tier, v.stepped_down), (QualityTier::Medium, true));
    assert!(v.reason.contains("High → Medium"), "{}", v.reason);
    let v = judge_probe(QualityTier::High, &result(300, 12.0), 60.0);
    assert_eq!((v.tier, v.stepped_down), (QualityTier::High, false));
    // Low holds the 30 fps floor (ADR-7882): kept under the desktop's 60 fps target.
    let v = judge_probe(QualityTier::Low, &result(300, 32.7), 60.0);
    assert_eq!((v.tier, v.stepped_down), (QualityTier::Low, false));
    assert!(v.reason.contains("within 33.3 ms"), "{}", v.reason);
    let v = judge_probe(QualityTier::VeryLow, &result(300, 52.7), 60.0);
    assert_eq!((v.tier, v.stepped_down), (QualityTier::VeryLow, false));
    assert!(v.reason.contains("lowest"), "{}", v.reason);
    let v = judge_probe(QualityTier::Ultra, &result(0, 0.0), 60.0);
    assert_eq!((v.tier, v.stepped_down), (QualityTier::Ultra, false));
    // Configurable target: 30 fps lets a 25 ms p95 pass.
    assert!(!judge_probe(QualityTier::High, &result(90, 25.0), 30.0).stepped_down);
}

/// The wall clock includes the vsync wait (≈ 16.7 ms under Fifo at 60 Hz): a probe that only had
/// the wall clock never steps down, however slow it measured.
#[test]
fn probe_verdict_never_steps_down_on_the_wall_clock() {
    let wall = ProbeResult {
        source: ProbeSource::Wall,
        ..result(300, 40.0)
    };
    let v = judge_probe(QualityTier::High, &wall, 60.0);
    assert_eq!((v.tier, v.stepped_down), (QualityTier::High, false));
    assert!(v.reason.contains("wall clock"), "{}", v.reason);
}

#[test]
fn percentile_is_nearest_rank() {
    assert_eq!(percentile(&[], 0.95), 0.0);
    let samples: Vec<f32> = (1..=100).map(|i| i as f32).collect();
    assert_eq!(percentile(&samples, 0.5), 50.0);
    assert_eq!(percentile(&samples, 0.95), 95.0);
    assert_eq!(percentile(&[3.0, 1.0, 2.0], 1.0), 3.0);
}

/// The default probe is 0.75 s of warm-up plus 2.25 s measured, and a request does not
/// start while the splash is up (ADR-173).
#[test]
fn probe_default_waits_until_the_splash_is_gone() {
    let config = ProbeConfig::default();
    assert_eq!(config.warmup, Duration::from_millis(750));
    assert_eq!(config.measure, Duration::from_millis(2250));
    assert_eq!(config.warmup_frames, ProbeConfig::MIN_WARMUP_FRAMES);
    assert!(!probe_start_allowed(false, false, false), "splash still up");
    assert!(
        !probe_start_allowed(true, true, false),
        "game-frame warm still pending"
    );
    assert!(
        !probe_start_allowed(true, false, true),
        "a scene warm is in progress"
    );
    assert!(probe_start_allowed(true, false, false));
    let mut probe = QualityProbe::default();
    probe.request(config);
    assert!(probe.wants_start());
    assert!(
        !probe.is_running(),
        "request arms the probe; it does not draw yet"
    );
}

/// The probe loop drops at least the configured warm-up time and frame count, then times the
/// measured window on the GPU clock (ADR-173).
#[test]
fn probe_run_drops_the_warmup_and_times_the_gpu() {
    let config = ProbeConfig {
        warmup: Duration::from_millis(100),
        warmup_frames: 30,
        measure: Duration::from_millis(400),
        timeout: Duration::from_secs(60),
    };
    let t0 = Instant::now();
    let mut run = ProbeRun::new(config, t0);
    // 40 slow first-frame transients inside 50 ms: past the frame count, not the time.
    for i in 0..40 {
        assert!(!run.frame(t0 + Duration::from_millis(i), 50.0, 52.0));
    }
    let mut t = t0;
    let mut done = false;
    for i in 0..60 {
        t = t0 + Duration::from_millis(100 + i * 10);
        // Measured frames 10 ms with a 20 ms tail, the GPU busy most of the interval.
        let gpu = if i % 10 == 0 { 20.0 } else { 10.0 };
        done = run.frame(t, gpu, 11.0);
        if done {
            break;
        }
    }
    assert!(done, "the measured window ends ({t:?})");
    let r = run.result((1920, 1080));
    assert_eq!(r.source, ProbeSource::Gpu);
    assert_eq!((r.width, r.height), (1920, 1080));
    assert!(r.frames >= 40, "{r:?}");
    assert_eq!(r.p50_ms, 10.0);
    assert_eq!(r.p95_ms, 20.0, "warm-up frames are dropped");
    assert!(r.gpu_busy.is_some_and(|b| b > 0.85), "{r:?}");
    assert!(!r.aborted && !r.timed_out);
}

/// The warm-up needs its frame count even after its time: a slow first second at a few
/// frames per second is all warm-up.
#[test]
fn probe_run_needs_the_warmup_frames() {
    let config = ProbeConfig {
        warmup: Duration::from_millis(100),
        measure: Duration::from_millis(100),
        ..ProbeConfig::default()
    };
    let t0 = Instant::now();
    let mut run = ProbeRun::new(config, t0);
    for i in 1..=ProbeConfig::MIN_WARMUP_FRAMES {
        assert!(!run.frame(t0 + Duration::from_secs(u64::from(i)), 90.0, 1000.0));
    }
    assert_eq!(run.result((64, 64)).frames, 0, "every frame so far is warm-up");
    assert!(run.result((64, 64)).timed_out);
}

#[test]
fn probe_without_a_scene_ends_after_its_timeout() {
    let mut probe = QualityProbe::default();
    probe.request(ProbeConfig {
        timeout: Duration::from_millis(10),
        ..ProbeConfig::with_seconds(1.0)
    });
    assert!(probe.wants_start());
    probe.poll(Instant::now());
    assert!(probe.is_active() && probe.take_result().is_none(), "not yet");
    probe.poll(Instant::now() + Duration::from_secs(1));
    let Some(r) = probe.take_result() else {
        panic!("a window without a scene still yields a result");
    };
    assert_eq!(r.frames, 0);
    assert!(r.timed_out);
    assert!(!judge_probe(QualityTier::High, &r, 60.0).stepped_down);
    assert!(!probe.is_active());
}

/// A level load cuts a probe short (ADR-173): an aborted result, never judged.
#[test]
fn an_aborted_probe_keeps_the_tier() {
    let mut probe = QualityProbe::default();
    assert!(!probe.abort(), "nothing to abort");
    probe.request(ProbeConfig::with_seconds(3.0));
    assert!(probe.abort());
    assert!(!probe.is_running() && !probe.wants_start());
    let Some(r) = probe.take_result() else {
        panic!("an aborted probe still yields a result");
    };
    assert!(r.aborted && r.frames == 0 && !r.timed_out, "{r:?}");
    let v = judge_probe(QualityTier::Ultra, &r, 60.0);
    assert_eq!((v.tier, v.stepped_down), (QualityTier::Ultra, false));
    assert!(v.reason.contains("aborted"), "{}", v.reason);
    assert_eq!(probe.runs(), 1);
    assert!(!probe.is_active());
}

/// A GPU that was busy for under half the frame interval (present- or CPU-bound, downclocked:
/// the RTX 3080 Ti sweep, 2026-10-10) never steps down; transients alone (a slow p95 over a
/// fast median) never step down either.
#[test]
fn probe_verdict_ignores_an_idle_gpu_and_transients() {
    let idle = ProbeResult {
        gpu_busy: Some(0.35),
        ..result(300, 24.0)
    };
    let v = judge_probe(QualityTier::High, &idle, 60.0);
    assert_eq!((v.tier, v.stepped_down), (QualityTier::High, false));
    assert!(v.reason.contains("busy"), "{}", v.reason);
    let busy = ProbeResult {
        gpu_busy: Some(0.9),
        ..result(300, 24.0)
    };
    assert!(judge_probe(QualityTier::High, &busy, 60.0).stepped_down);
    let spiky = ProbeResult {
        p50_ms: 8.0,
        ..result(300, 24.0)
    };
    let v = judge_probe(QualityTier::High, &spiky, 60.0);
    assert!(!v.stepped_down, "{}", v.reason);
}

/// The whole probe on lavapipe: started on a warm worker with the canonical frame, it warms
/// a renderer of its own, draws back to back and hands over a measured result.
#[test]
fn probe_runs_on_a_warm_worker_and_measures() {
    let device = crate::device::test_device();
    let queue = crate::PipelineWarmQueue::new(1);
    queue.start(&device);
    let runtime = crate::FrameRenderer::new();
    let mut probe = QualityProbe::default();
    probe.request(ProbeConfig {
        warmup: Duration::from_millis(50),
        warmup_frames: 3,
        measure: Duration::from_millis(150),
        timeout: Duration::from_secs(300),
    });
    let frame = crate::pipeline_warm::canonical_frame();
    probe.start(
        &queue,
        &runtime,
        &frame,
        wgpu::TextureFormat::Rgba8UnormSrgb,
        (96, 64),
    );
    assert!(probe.is_running());
    let deadline = Instant::now() + Duration::from_secs(300);
    let result = loop {
        probe.poll(Instant::now());
        if let Some(r) = probe.take_result() {
            break r;
        }
        assert!(Instant::now() < deadline, "the probe never finished");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(
        result.frames > 0 && !result.timed_out && !result.aborted,
        "{result:?}"
    );
    assert_eq!((result.width, result.height), (96, 64));
    assert_eq!(runtime.cache_sizes().0, 0, "the runtime renderer drew nothing");
    queue.shutdown();
}

/// What this machine's GPU detects as (real-GPU check on the farm boxes:
/// `cargo test -p regia_render --lib live_adapter_detection -- --ignored --nocapture`).
#[test]
#[ignore = "needs a GPU; prints the live adapter's detection"]
fn live_adapter_detection() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no render device: {err}"),
    };
    let adapter = AdapterProfile::from_device(&device);
    let detection = detect(&adapter, &GpuTierTable::builtin());
    println!("AUTOQ adapter: {adapter:?}");
    println!("AUTOQ reason: {}", detection.reason);
    assert!(!detection.reason.is_empty());
}

#[test]
fn desktop_low_turns_the_terrain_virtual_texture_on_only_for_6_gb_discrete_gpus() {
    // ADR-157 addendum v0.1.2: Desktop Low on a discrete desktop GPU with ≥ 6 GB known VRAM.
    let gtx_1080 = AdapterProfile::named("NVIDIA GeForce GTX 1080", NVIDIA, Some(8192));
    let gtx_1060_6 = AdapterProfile::named("NVIDIA GeForce GTX 1060 6GB", NVIDIA, Some(6144));
    let gtx_1650 = AdapterProfile::named("NVIDIA GeForce GTX 1650", NVIDIA, Some(4096));
    let unknown = AdapterProfile::named("NVIDIA GeForce GTX 1080", NVIDIA, None);
    let mut integrated = AdapterProfile::named("AMD Radeon Graphics", AMD, Some(16384));
    integrated.class = DeviceClass::Integrated;
    let mut lavapipe = AdapterProfile::named("llvmpipe (LLVM 19.1.0, 256 bits)", 0x10005, Some(16384));
    lavapipe.class = DeviceClass::Cpu;
    let mut phone = AdapterProfile::named("Mali-G78", ARM, Some(8192));
    phone.android = true;

    assert_eq!(
        tier_of("NVIDIA GeForce GTX 1080", NVIDIA, Some(8192)),
        QualityTier::Low
    );
    let low = QualityTier::Low;
    assert!(low.terrain_vt_default_on(&gtx_1080, "linux"));
    assert!(low.terrain_vt_default_on(&gtx_1060_6, "linux"));
    for adapter in [&gtx_1650, &unknown, &integrated, &lavapipe, &phone] {
        assert!(!low.terrain_vt_default_on(adapter, "linux"), "{}", adapter.name);
    }
    for tier in QualityTier::ALL.into_iter().filter(|t| *t != QualityTier::Low) {
        assert!(!tier.terrain_vt_default_on(&gtx_1080, "linux"), "{tier}");
    }
    // The build's own OS decides at runtime.
    assert_eq!(low.terrain_vt_default(&gtx_1080), cfg!(target_os = "linux"));
}

/// ADR-157 addendum v0.1.2 (review of v012-vt-horizon): the default is Linux only (no Windows
/// driver has run the texture; Windows enumerates Vulkan first, so its VRAM is known) and skips
/// AMD (no RADV measurement yet). NVIDIA and Intel on Linux keep it.
#[test]
fn the_terrain_vt_default_is_linux_only_and_skips_amd() {
    let low = QualityTier::Low;
    let gtx_1080 = AdapterProfile::named("NVIDIA GeForce GTX 1080", NVIDIA, Some(8_192));
    let mut g31 = AdapterProfile::named("Intel(R) Graphics (BMG G31)", INTEL, Some(32_624));
    g31.device = 0xe222;
    let b580 = AdapterProfile::named("Intel(R) Arc(TM) B580 Graphics", INTEL, Some(12_216));
    for adapter in [&gtx_1080, &g31, &b580] {
        assert_eq!(detect(adapter, &table()).tier, low, "{}", adapter.name);
        assert!(
            low.terrain_vt_default_on(adapter, "linux"),
            "{} on Linux",
            adapter.name
        );
        for os in ["windows", "macos", "android"] {
            assert!(
                !low.terrain_vt_default_on(adapter, os),
                "{} on {os}",
                adapter.name
            );
        }
    }
    // AMD 8 GB cards that detect Low get no texture on any OS.
    for name in [
        "AMD Radeon RX 6600",
        "AMD Radeon RX 580 Series",
        "AMD Radeon RX 5500 XT",
    ] {
        let rx = AdapterProfile::named(name, AMD, Some(8_176));
        assert_eq!(detect(&rx, &table()).tier, low, "{name}");
        for os in ["linux", "windows"] {
            assert!(!low.terrain_vt_default_on(&rx, os), "{name} on {os}");
        }
        let mut settings = RenderSettings::default();
        low.apply_for_gpu(&mut settings, Some(&rx));
        assert!(!settings.profile.terrain_budget.virtual_texture, "{name}");
    }
}

/// Intel Arc rows (arc-sweep, 2026-10-11): the measured BMG G31 by PCI id and by Mesa's name,
/// the B580-class and Pro B60-class (BMG G21) cards by class, all Low, and Low turns the terrain
/// virtual texture on for each of them (discrete, >= 6 GB).
#[test]
fn intel_arc_cards_get_low_with_the_terrain_virtual_texture() {
    let table = table();
    let mut g31 = AdapterProfile::named("Intel(R) Graphics (BMG G31)", INTEL, Some(32_624));
    g31.device = 0xe222;
    let d = detect(&g31, &table);
    assert_eq!(d.rule, "intel_arc_bmg_g31", "{}", d.reason);
    assert_eq!(d.tier, QualityTier::Low, "{}", d.reason);
    assert!(QualityTier::Low.terrain_vt_default_on(&g31, "linux"));
    // The PCI id alone (a driver that reports another name) and the name alone both match.
    let mut by_id = AdapterProfile::named("Intel(R) Graphics", INTEL, Some(32_624));
    by_id.device = 0xe222;
    assert_eq!(detect(&by_id, &table).rule, "intel_arc_bmg_g31");
    let by_name = AdapterProfile::named("Intel(R) Graphics (BMG G31)", INTEL, Some(32_624));
    assert_eq!(detect(&by_name, &table).rule, "intel_arc_bmg_g31");
    let cases: &[(&str, u64)] = &[
        ("Intel(R) Graphics (BMG G21)", 24_480),
        ("Intel(R) Arc(TM) Pro B60 Graphics (BMG G21)", 24_480),
        ("Intel(R) Arc(TM) B580 Graphics", 12_216),
        ("Intel(R) Arc(TM) B570 Graphics", 10_168),
        ("Intel(R) Arc(TM) A770 Graphics", 16_288),
        ("Intel(R) Arc(TM) A750 Graphics", 8_144),
    ];
    for &(name, vram) in cases {
        let adapter = AdapterProfile::named(name, INTEL, Some(vram));
        let d = detect(&adapter, &table);
        assert_eq!(d.rule, "intel_arc_mid", "{name}: {}", d.reason);
        assert_eq!(d.tier, QualityTier::Low, "{name}: {}", d.reason);
        assert!(
            QualityTier::Low.terrain_vt_default_on(&adapter, "linux"),
            "{name}"
        );
        let mut settings = RenderSettings::default();
        d.tier.apply_for_gpu(&mut settings, Some(&adapter));
        assert_eq!(
            settings.profile.terrain_budget.virtual_texture,
            cfg!(target_os = "linux"),
            "{name}"
        );
    }
}

#[test]
fn the_terrain_vt_lever_keeps_the_tier_and_preset_recognisable() {
    let gtx_1080 = AdapterProfile::named("NVIDIA GeForce GTX 1080", NVIDIA, Some(8192));
    let mut settings = RenderSettings::default();
    QualityTier::Low.apply_for_gpu(&mut settings, Some(&gtx_1080));
    // (Linux only: ADR-157 addendum v0.1.2.)
    assert_eq!(
        settings.profile.terrain_budget.virtual_texture,
        cfg!(target_os = "linux")
    );
    assert_eq!(QualityTier::of_settings(&settings), Some(QualityTier::Low));
    assert_eq!(settings.preset(), Some(RenderProfilePreset::DesktopLow));

    // Switching to High (or applying without a known adapter) clears it again.
    QualityTier::High.apply_for_gpu(&mut settings, Some(&gtx_1080));
    assert!(!settings.profile.terrain_budget.virtual_texture);
    QualityTier::Low.apply_for_gpu(&mut settings, None);
    assert!(!settings.profile.terrain_budget.virtual_texture);
    assert_eq!(settings.profile, QualityTier::Low.profile());

    // No preset turns it on: the goldens' frames are unchanged.
    for preset in RenderProfilePreset::ALL {
        assert!(
            !RenderProfile::from_preset(preset).terrain_budget.virtual_texture,
            "{preset:?}"
        );
    }
}

/// ADR-2122: the default [`TerrainVtPolicy`] is the measured ADR-157 rule. No tier but Low, no
/// OS but Linux and no AMD card gets the texture, whatever the card.
#[test]
fn the_default_terrain_vt_policy_is_the_measured_rule() {
    let policy = TerrainVtPolicy::default();
    assert_eq!(
        policy,
        TerrainVtPolicy {
            high: false,
            windows: false,
            unmeasured_vendors: false
        }
    );
    let rtx_4090 = AdapterProfile::named("NVIDIA GeForce RTX 4090", NVIDIA, Some(24_564));
    let gtx_1080 = AdapterProfile::named("NVIDIA GeForce GTX 1080", NVIDIA, Some(8_192));
    let rx_7900 = AdapterProfile::named("AMD Radeon RX 7900 XTX (RADV NAVI31)", AMD, Some(24_560));
    let rx_6600 = AdapterProfile::named("AMD Radeon RX 6600", AMD, Some(8_176));
    for adapter in [&rtx_4090, &gtx_1080, &rx_7900, &rx_6600] {
        for tier in QualityTier::ALL {
            for os in ["linux", "windows", "macos", "android"] {
                let want = tier == QualityTier::Low && os == "linux" && adapter.vendor != AMD;
                assert_eq!(
                    tier.terrain_vt_default_with(adapter, os, &policy),
                    want,
                    "{} {tier} {os}",
                    adapter.name
                );
                assert_eq!(
                    tier.terrain_vt_default_on(adapter, os),
                    want,
                    "{} {tier} {os}",
                    adapter.name
                );
            }
        }
    }
}

/// ADR-2122: `TerrainVtPolicy::high` turns the texture on for Desktop High on discrete cards with
/// at least 11.5 GB (a 12 GB card qualifies, a 10 GB one does not). Medium and Ultra never get
/// it. The tier and the preset still read as High: the lever is masked.
#[test]
fn the_high_policy_turns_the_terrain_vt_on_for_12_gb_high_cards() {
    let policy = TerrainVtPolicy {
        high: true,
        ..TerrainVtPolicy::default()
    };
    let rtx_4070 = AdapterProfile::named("NVIDIA GeForce RTX 4070", NVIDIA, Some(12_282));
    let rtx_3080_10 = AdapterProfile::named("NVIDIA GeForce RTX 3080", NVIDIA, Some(10_240));
    let mut integrated = AdapterProfile::named("Intel(R) Graphics", INTEL, Some(32_000));
    integrated.class = DeviceClass::Integrated;
    let high = QualityTier::High;
    assert!(high.terrain_vt_default_with(&rtx_4070, "linux", &policy));
    assert!(!high.terrain_vt_default_with(&rtx_3080_10, "linux", &policy));
    assert!(!high.terrain_vt_default_with(&integrated, "linux", &policy));
    assert!(!high.terrain_vt_default_with(&rtx_4070, "windows", &policy));
    for tier in [QualityTier::Medium, QualityTier::Ultra, QualityTier::Deck] {
        assert!(
            !tier.terrain_vt_default_with(&rtx_4070, "linux", &policy),
            "{tier}"
        );
    }
    // Low keeps its own 6 GB floor under the High policy.
    let gtx_1060_6 = AdapterProfile::named("NVIDIA GeForce GTX 1060 6GB", NVIDIA, Some(6_144));
    assert!(QualityTier::Low.terrain_vt_default_with(&gtx_1060_6, "linux", &policy));

    let mut settings = RenderSettings::default();
    high.apply_for_gpu_with(&mut settings, Some(&rtx_4070), &policy);
    let on = cfg!(target_os = "linux");
    assert_eq!(settings.profile.terrain_budget.virtual_texture, on);
    assert_eq!(QualityTier::of_settings(&settings), Some(QualityTier::High));
    assert_eq!(settings.preset(), Some(RenderProfilePreset::DesktopHigh));
    // The default policy leaves High's frame alone.
    let mut plain = RenderSettings::default();
    high.apply_for_gpu(&mut plain, Some(&rtx_4070));
    assert!(!plain.profile.terrain_budget.virtual_texture);
}

/// ADR-2122: `windows` admits Windows builds (not macOS or Android), with DX12's DXGI memory
/// standing in where Vulkan's heap is unknown; `unmeasured_vendors` admits AMD.
#[test]
fn the_windows_and_amd_opt_ins_lift_only_their_own_rule() {
    let low = QualityTier::Low;
    let gtx_1080 = AdapterProfile::named("NVIDIA GeForce GTX 1080", NVIDIA, Some(8_192));
    let windows = TerrainVtPolicy {
        windows: true,
        ..TerrainVtPolicy::default()
    };
    assert!(low.terrain_vt_default_with(&gtx_1080, "windows", &windows));
    assert!(low.terrain_vt_default_with(&gtx_1080, "linux", &windows));
    for os in ["macos", "android", "ios"] {
        assert!(!low.terrain_vt_default_with(&gtx_1080, os, &windows), "{os}");
    }
    // A DX12 adapter: Vulkan's heap is unknown; DXGI's dedicated memory decides.
    let mut dx12 = AdapterProfile::named("NVIDIA GeForce GTX 1080", NVIDIA, None);
    dx12.backend = "Dx12".into();
    assert!(!low.terrain_vt_default_with(&dx12, "windows", &windows));
    dx12.dx12_vram_mb = Some(8_110);
    assert!(low.terrain_vt_default_with(&dx12, "windows", &windows));
    dx12.dx12_vram_mb = Some(4_052);
    assert!(!low.terrain_vt_default_with(&dx12, "windows", &windows));
    // The DXGI number never feeds tier detection: VRAM stays unknown there.
    let mut dx12_big = AdapterProfile::named("Some Future GPU", NVIDIA, None);
    let unknown = detect(&dx12_big, &table());
    dx12_big.dx12_vram_mb = Some(2_048);
    assert_eq!(detect(&dx12_big, &table()).tier, unknown.tier);

    let rx_6600 = AdapterProfile::named("AMD Radeon RX 6600", AMD, Some(8_176));
    assert!(!low.terrain_vt_default_with(&rx_6600, "linux", &windows));
    let amd = TerrainVtPolicy {
        unmeasured_vendors: true,
        ..TerrainVtPolicy::default()
    };
    assert!(low.terrain_vt_default_with(&rx_6600, "linux", &amd));
    assert!(!low.terrain_vt_default_with(&rx_6600, "windows", &amd));
    let mut rx_apu = AdapterProfile::named("AMD Radeon Graphics", AMD, Some(16_384));
    rx_apu.class = DeviceClass::Integrated;
    assert!(!low.terrain_vt_default_with(&rx_apu, "linux", &amd));
    // Serialized names (the agent's `quality.set_terrain_vt_policy`); absent fields are off.
    let parsed: TerrainVtPolicy = serde_json::from_str(r#"{"high": true}"#).expect("policy json");
    assert_eq!(
        parsed,
        TerrainVtPolicy {
            high: true,
            ..TerrainVtPolicy::default()
        }
    );
}

/// ADR-2122 budget: Desktop High's terrain virtual texture budget is a small share of the
/// Desktop High memory budget, and the smallest card the High policy admits (found by sweeping
/// the decision, not read from the constant) holds that whole budget plus the desktop's reserve.
/// The measured texture memory is checked against the same budget by `regia_terrain`'s
/// `terrain_vt_tiers`.
#[test]
fn the_high_terrain_vt_fits_the_desktop_high_memory_budget() {
    use crate::memory_budget::{BudgetTier, MemoryBudget};
    const MIB: u64 = 1024 * 1024;
    let budget = MemoryBudget::for_tier(BudgetTier::DesktopHigh);
    // At most 7.5 % of the Desktop High budget.
    assert!(QualityTier::TERRAIN_VT_HIGH_BUDGET_BYTES * 40 <= budget.vram * 3);
    let high = TerrainVtPolicy {
        high: true,
        ..TerrainVtPolicy::default()
    };
    let floor = |tier: QualityTier, policy: &TerrainVtPolicy| {
        (0..=32_768u64).find(|&mb| {
            let card = AdapterProfile::named("NVIDIA GeForce RTX 4070", NVIDIA, Some(mb));
            tier.terrain_vt_default_with(&card, "linux", policy)
        })
    };
    let high_floor = floor(QualityTier::High, &high).expect("the High policy admits some card");
    // The whole budget plus the 1.5 GiB the desktop keeps.
    assert!(
        high_floor * MIB >= budget.vram + 1536 * MIB,
        "{high_floor} MB admits a card below the Desktop High budget {}",
        budget.vram
    );
    assert_eq!(high_floor, QualityTier::TERRAIN_VT_HIGH_MIN_VRAM_MB);
    // A 12 GB card's reported size (12 282 MB) clears it, a 10 GB one does not.
    assert!(high_floor <= 12_282 && high_floor > 10_240);
    // Low keeps its lower floor, under either policy; the default policy admits no High card.
    let low_floor = floor(QualityTier::Low, &TerrainVtPolicy::default());
    assert_eq!(low_floor, floor(QualityTier::Low, &high));
    assert!(low_floor.is_some_and(|mb| mb < high_floor));
    assert_eq!(floor(QualityTier::High, &TerrainVtPolicy::default()), None);
}

/// ADR-2122: the lever carries the late-depth bit only under `windows`; it is masked like the
/// lever, and clearing the lever clears it. No preset and no default policy sets it.
#[test]
fn the_late_depth_bit_follows_the_windows_opt_in() {
    let gtx_1080 = AdapterProfile::named("NVIDIA GeForce GTX 1080", NVIDIA, Some(8_192));
    for preset in RenderProfilePreset::ALL {
        assert!(
            !RenderProfile::from_preset(preset).terrain_budget.vt_late_depth,
            "{preset:?}"
        );
    }
    for tier in QualityTier::ALL {
        let mut settings = RenderSettings::default();
        tier.apply_for_gpu(&mut settings, Some(&gtx_1080));
        assert!(!settings.profile.terrain_budget.vt_late_depth, "{tier}");
    }
    let windows = TerrainVtPolicy {
        windows: true,
        ..TerrainVtPolicy::default()
    };
    assert!(windows.late_depth());
    for policy in [
        TerrainVtPolicy::default(),
        TerrainVtPolicy {
            high: true,
            unmeasured_vendors: true,
            ..TerrainVtPolicy::default()
        },
    ] {
        assert!(!policy.late_depth(), "{policy:?}");
    }
    let mut settings = RenderSettings::default();
    QualityTier::Low.apply_for_gpu_with(&mut settings, Some(&gtx_1080), &windows);
    let admitted = cfg!(any(target_os = "linux", target_os = "windows"));
    let budget = settings.profile.terrain_budget;
    assert_eq!(
        (budget.virtual_texture, budget.vt_late_depth),
        (admitted, admitted)
    );
    assert_eq!(QualityTier::of_settings(&settings), Some(QualityTier::Low));
    assert_eq!(settings.preset(), Some(RenderProfilePreset::DesktopLow));
    // Off together with the lever.
    let mut budget = settings.profile.terrain_budget;
    windows.set_lever(&mut budget, false);
    assert!(!budget.virtual_texture && !budget.vt_late_depth);
    TerrainVtPolicy::default().set_lever(&mut budget, true);
    assert!(budget.virtual_texture && !budget.vt_late_depth);
}

/// Audit G render5 #5: AMD laptop names carry an `M` or `S` suffix and no "laptop", so the
/// desktop rows' substrings (`rx 7900`, `rx 6800`) took them to the desktop Ultra and High.
#[test]
fn amd_laptop_skus_do_not_take_desktop_ultra() {
    let laptop = |name: &str| tier_of(name, AMD, Some(16_368));
    assert_eq!(laptop("AMD Radeon RX 7900M"), QualityTier::High);
    assert_eq!(laptop("AMD Radeon RX 7900M (RADV NAVI31)"), QualityTier::High);
    for name in [
        "AMD Radeon RX 7800M",
        "AMD Radeon RX 7700S",
        "AMD Radeon RX 7600M XT",
        "AMD Radeon RX 6850M XT",
        "AMD Radeon RX 6800M (RADV NAVI22)",
    ] {
        assert_eq!(laptop(name), QualityTier::Medium, "{name}");
    }
    for name in [
        "AMD Radeon RX 7600M",
        "AMD Radeon RX 7600S",
        "AMD Radeon RX 6800S",
        "AMD Radeon RX 6700M",
        "AMD Radeon RX 6700S",
        "AMD Radeon RX 6650M",
        "AMD Radeon RX 6600M",
        "AMD Radeon RX 6600S",
        "AMD Radeon RX 6550M",
        "AMD Radeon RX 6500M",
        "AMD Radeon RX 5700M",
        "AMD Radeon RX 5600M",
        "AMD Radeon RX 5500M",
    ] {
        assert_eq!(laptop(name), QualityTier::Low, "{name}");
    }
    // The desktop parts keep their rows.
    assert_eq!(laptop("AMD Radeon RX 7900 XTX"), QualityTier::Ultra);
    assert_eq!(laptop("AMD Radeon RX 7900 XTX (RADV NAVI31)"), QualityTier::Ultra);
    assert_eq!(laptop("AMD Radeon RX 7900 GRE"), QualityTier::High);
    assert_eq!(laptop("AMD Radeon RX 6800 XT"), QualityTier::High);
    assert_eq!(laptop("AMD Radeon RX 7600 XT"), QualityTier::Medium);
    assert_eq!(laptop("AMD Radeon RX 6600"), QualityTier::Low);
}

/// Audit G render5 #6: Adreno 702 and 710 (Snapdragon 4- and 6-class) contain "adreno (tm) 7"
/// and took the Snapdragon 8-class row's Mobile High.
#[test]
fn adreno_710_and_702_stay_mobile_low() {
    let table = table();
    let phone = |name: &str| {
        let mut a = AdapterProfile::named(name, QUALCOMM, None);
        a.class = DeviceClass::Integrated;
        a.android = true;
        a.block_compression = false;
        detect(&a, &table).tier
    };
    for name in [
        "Adreno (TM) 702",
        "Adreno (TM) 710",
        "Adreno (TM) 810",
        "Adreno 710",
    ] {
        assert_eq!(phone(name), QualityTier::MobileLow, "{name}");
    }
    for name in [
        "Adreno (TM) 730",
        "Adreno (TM) 735",
        "Adreno (TM) 740",
        "Adreno (TM) 750",
        "Adreno (TM) 830",
        "Adreno (TM) 840",
        "Adreno 740",
    ] {
        assert_eq!(phone(name), QualityTier::MobileHigh, "{name}");
    }
}

/// Audit G render5 #7: with the dense-scene default on, `Medium.apply` copied Desktop High (the
/// switch turned the visibility path on), then wrote Medium's three overrides, leaving the path on
/// a row the default treats as unmeasured and a profile no tier produces.
#[test]
fn medium_apply_with_dense_default_leaves_the_path_off() {
    use crate::{VisibilityDefault, VisibilityDefaultState, VisibilityPath};
    for apply in [
        (|s: &mut RenderSettings| QualityTier::Medium.apply(s)) as fn(&mut RenderSettings),
        |s| QualityTier::Medium.apply_for_gpu_with(s, None, &TerrainVtPolicy::default()),
    ] {
        let mut settings = RenderSettings {
            visibility_default: VisibilityDefault::Dense,
            ..RenderSettings::from_profile(RenderProfilePreset::DesktopUltra)
        };
        apply(&mut settings);
        assert!(
            !settings.profile.visibility_buffer,
            "Medium kept the dense default's path"
        );
        assert_eq!(settings.profile.visibility_path, VisibilityPath::DEFAULT);
        assert_eq!(
            settings.visibility_default_state(),
            VisibilityDefaultState::PresetUnmeasured
        );
        assert_eq!(QualityTier::of_settings(&settings), Some(QualityTier::Medium));
    }
    // High, a measured row, still takes the path; back to Medium drops it again.
    let mut settings = RenderSettings {
        visibility_default: VisibilityDefault::Dense,
        ..RenderSettings::default()
    };
    QualityTier::High.apply(&mut settings);
    assert!(settings.profile.visibility_buffer);
    assert_eq!(settings.profile.visibility_path, VisibilityPath::DENSE_DEFAULT);
    QualityTier::Medium.apply(&mut settings);
    assert!(!settings.profile.visibility_buffer);
    assert_eq!(QualityTier::of_settings(&settings), Some(QualityTier::Medium));
}

// ---- Very Low, the step below Low (ADR-7882) ------------------------------------------------

/// `Low` steps down to `VeryLow` and nothing is below it; `ALL` keeps the seven tiers it had
/// (procedural bakes and quality pins are keyed on them) and `EVERY` adds Very Low below Low.
#[test]
fn low_steps_down_to_very_low_and_nothing_is_below_it() {
    assert_eq!(QualityTier::Low.step_down(), Some(QualityTier::VeryLow));
    assert_eq!(QualityTier::VeryLow.step_down(), None);
    assert_eq!(QualityTier::MobileLow.step_down(), None);
    assert_eq!(
        QualityTier::ALL,
        [
            QualityTier::MobileLow,
            QualityTier::MobileHigh,
            QualityTier::Deck,
            QualityTier::Low,
            QualityTier::Medium,
            QualityTier::High,
            QualityTier::Ultra,
        ]
    );
    let mut every: Vec<QualityTier> = QualityTier::ALL.to_vec();
    every.insert(3, QualityTier::VeryLow);
    assert_eq!(
        QualityTier::EVERY.to_vec(),
        every,
        "cheapest first, Very Low just below Low"
    );
    for pair in QualityTier::EVERY.windows(2) {
        assert!(pair[0].rank() < pair[1].rank(), "{pair:?}");
    }
    assert!(QualityTier::VeryLow.is_desktop());
    assert_eq!(QualityTier::VeryLow.content_tier(), QualityTier::Low);
    for tier in QualityTier::ALL {
        assert_eq!(tier.content_tier(), tier, "{tier}");
    }
    assert_eq!(QualityTier::VeryLow.for_platform(true), QualityTier::MobileLow);
    assert_eq!(QualityTier::VeryLow.for_platform(false), QualityTier::VeryLow);
    assert_eq!(QualityTier::from_name("very_low"), Some(QualityTier::VeryLow));
    assert_eq!(QualityTier::from_name("VeryLow"), Some(QualityTier::VeryLow));
    assert_eq!(
        QualityTier::VeryLow.default_target_fps(),
        QualityTier::VERY_LOW_TRIGGER_FPS
    );
    assert_eq!(QualityTier::VeryLow.preset(), RenderProfilePreset::DesktopLow);
}

/// Very Low is Desktop Low at a third of the scale with SGSR, Low itself is untouched, and the profile still
/// reads as Desktop Low so systems that pick their own tier by preset stay on Low's
/// (profile-field-breaks-preset).
#[test]
fn very_low_is_desktop_low_drawn_at_a_third_and_reads_as_desktop_low() {
    let low = RenderProfile::from_preset(RenderProfilePreset::DesktopLow);
    assert_eq!(
        QualityTier::Low.profile(),
        low,
        "Low is still exactly Desktop Low"
    );
    let very_low = QualityTier::VeryLow.profile();
    assert_eq!(very_low.render_scale, QualityTier::VERY_LOW_RENDER_SCALE);
    assert_eq!(very_low.spatial_upscaler, crate::SpatialUpscaler::Sgsr);
    let mut columns = low.clone();
    QualityTier::very_low_columns(&mut columns);
    assert_eq!(columns, very_low);
    assert_ne!(very_low, low);

    for (adapter, label) in [
        (None, "no adapter"),
        (
            Some(AdapterProfile::named(
                "NVIDIA GeForce GTX 1080",
                NVIDIA,
                Some(8192),
            )),
            "GTX 1080",
        ),
    ] {
        let mut settings = RenderSettings {
            gi_look: Some(GiLook::Balanced),
            ..RenderSettings::default()
        };
        QualityTier::VeryLow.apply_for_gpu(&mut settings, adapter.as_ref());
        assert_eq!(
            QualityTier::of_settings(&settings),
            Some(QualityTier::VeryLow),
            "{label}"
        );
        assert_eq!(
            settings.preset(),
            Some(RenderProfilePreset::DesktopLow),
            "{label}: Very Low reads as Desktop Low"
        );
        assert_eq!(
            settings.gi_look,
            Some(GiLook::Balanced),
            "the player's look is kept"
        );
        assert_eq!(settings.profile.render_scale, QualityTier::VERY_LOW_RENDER_SCALE);
        // And back: Low is Low again, at Low's scale.
        QualityTier::Low.apply_for_gpu(&mut settings, adapter.as_ref());
        assert_eq!(
            QualityTier::of_settings(&settings),
            Some(QualityTier::Low),
            "{label}"
        );
        assert_eq!(settings.profile.render_scale, low.render_scale);
    }
    // Runtime scale changes preserve Very Low's preset classification.
    let mut settings = RenderSettings::default();
    QualityTier::VeryLow.apply(&mut settings);
    settings.profile.render_scale = 0.4;
    assert_eq!(settings.preset(), Some(RenderProfilePreset::DesktopLow));
    assert_eq!(QualityTier::of_settings(&settings), None);
    // A genuine quality-column override still makes it a custom profile.
    settings.profile.csm_resolution = 123;
    assert_eq!(settings.preset(), None);
    assert_eq!(QualityTier::of_settings(&settings), None);
    // Every preset row is still its own preset.
    for preset in RenderProfilePreset::ALL {
        assert_eq!(
            RenderSettings::from_profile(preset).preset(),
            Some(preset),
            "{preset:?}"
        );
    }
}

/// The probe takes Low down to Very Low only when Low misses the 30 fps floor (or a lower host
/// target), on sustained GPU time; a Low that holds 30 fps keeps Low under the desktop's 60 fps
/// target, and Very Low never steps further.
#[test]
fn judge_probe_takes_low_to_very_low_only_below_30_fps() {
    // The N150 class: Desktop Low at ~68 ms (perf-log, 2026-11-14) misses 30 fps.
    let slow = ProbeResult {
        p50_ms: 68.0,
        ..result(120, 75.0)
    };
    let v = judge_probe(QualityTier::Low, &slow, 60.0);
    assert_eq!((v.tier, v.stepped_down), (QualityTier::VeryLow, true));
    assert!((v.budget_ms - 1000.0 / 30.0).abs() < 1e-3, "{}", v.budget_ms);
    assert!(v.reason.contains("Low → VeryLow"), "{}", v.reason);
    assert!(v.reason.contains("misses 30 fps"), "{}", v.reason);
    // Just over the floor, sustained: steps down.
    let v = judge_probe(QualityTier::Low, &result(300, 36.0), 60.0);
    assert_eq!(
        (v.tier, v.stepped_down),
        (QualityTier::VeryLow, true),
        "{}",
        v.reason
    );
    // Between 60 and 30 fps (a GTX 1080 on a heavy map, 39 fps): Low is kept.
    let v = judge_probe(QualityTier::Low, &result(300, 25.6), 60.0);
    assert_eq!(
        (v.tier, v.stepped_down),
        (QualityTier::Low, false),
        "{}",
        v.reason
    );
    // A host's own lower target lowers the floor with it: 20 fps lets a 45 ms p95 pass.
    let v = judge_probe(QualityTier::Low, &result(300, 45.0), 20.0);
    assert_eq!(
        (v.tier, v.stepped_down),
        (QualityTier::Low, false),
        "{}",
        v.reason
    );
    assert!((v.budget_ms - 50.0).abs() < 1e-3);
    // A 30 fps host target is the floor itself.
    assert!(judge_probe(QualityTier::Low, &result(300, 36.0), 30.0).stepped_down);
    // Transients over a fast median, a wall-clock-only probe and an idle GPU keep Low.
    let spiky = ProbeResult {
        p50_ms: 20.0,
        ..result(300, 40.0)
    };
    assert!(!judge_probe(QualityTier::Low, &spiky, 60.0).stepped_down);
    let wall = ProbeResult {
        source: ProbeSource::Wall,
        ..slow.clone()
    };
    assert!(!judge_probe(QualityTier::Low, &wall, 60.0).stepped_down);
    let idle = ProbeResult {
        gpu_busy: Some(0.3),
        ..slow.clone()
    };
    assert!(!judge_probe(QualityTier::Low, &idle, 60.0).stepped_down);
    // The other steps keep their 60 fps budget.
    let v = judge_probe(QualityTier::Medium, &result(300, 21.9), 60.0);
    assert_eq!((v.tier, v.stepped_down), (QualityTier::Low, true));
    assert!((v.budget_ms - 1000.0 / 60.0).abs() < 1e-3);
    // Very Low is the bottom.
    let v = judge_probe(QualityTier::VeryLow, &slow, 60.0);
    assert_eq!((v.tier, v.stepped_down), (QualityTier::VeryLow, false));
    assert!(v.reason.contains("lowest"), "{}", v.reason);
}

/// An adapter that `rule` matches: its vendor, class and first device id, a name made of its
/// first `any` and every `all` string.
fn adapter_for_rule(rule: &GpuRule, vram_mb: Option<u64>) -> AdapterProfile {
    let mut name: Vec<&str> = rule.any.first().map(String::as_str).into_iter().collect();
    name.extend(rule.all.iter().map(String::as_str));
    let mut adapter = AdapterProfile::named(&name.join(" "), rule.vendor.unwrap_or(0), vram_mb);
    adapter.device = rule.device_ids.first().copied().unwrap_or(0);
    adapter.class = rule.class.unwrap_or(DeviceClass::Discrete);
    adapter.android = matches!(rule.tier, QualityTier::MobileLow | QualityTier::MobileHigh);
    adapter
}

/// No GPU that the table put on Low (or any other tier) before Very Low existed moves, except
/// the rows that name Very Low themselves: every row's adapter, at several VRAM sizes, detects the
/// same with and without the Very Low rows; and the VRAM floors never step below Low.
#[test]
fn no_gpu_that_holds_low_today_moves_to_very_low() {
    let table = table();
    let mut before = table.clone();
    before.rules.retain(|r| r.tier != QualityTier::VeryLow);
    let very_low_rows: Vec<&GpuRule> = table
        .rules
        .iter()
        .filter(|r| r.tier == QualityTier::VeryLow)
        .collect();
    let mut checked = 0;
    for rule in &table.rules {
        for vram in [None, Some(1_024), Some(4_096), Some(8_192), Some(16_384)] {
            let adapter = adapter_for_rule(rule, vram);
            assert!(
                rule.matches(&adapter),
                "{}: the synthetic adapter must match",
                rule.id
            );
            let old = detect(&adapter, &before);
            let new = detect(&adapter, &table);
            if very_low_rows.iter().any(|r| r.matches(&adapter)) {
                assert_eq!(new.tier, QualityTier::VeryLow, "{}: {}", rule.id, new.reason);
                continue;
            }
            assert_eq!(new, old, "{} at {vram:?} MB moved", rule.id);
            assert_ne!(new.tier, QualityTier::VeryLow, "{}", rule.id);
            checked += 1;
        }
    }
    assert!(checked > 100, "only {checked} adapters checked");
    // The fallback's classes too.
    for (name, vendor, class) in [
        ("Moore Threads iGPU", 0x1ed5, DeviceClass::Integrated),
        ("llvmpipe (LLVM 19.1.7, 256 bits)", 0x10005, DeviceClass::Cpu),
        ("Some Virtual GPU", 0x1af4, DeviceClass::Virtual),
        ("Unknown Discrete", 0x1234, DeviceClass::Discrete),
    ] {
        let mut adapter = AdapterProfile::named(name, vendor, Some(2_048));
        adapter.class = class;
        assert_eq!(detect(&adapter, &table), detect(&adapter, &before), "{name}");
        assert_eq!(detect(&adapter, &table).tier, QualityTier::Low, "{name}");
    }
    // A table with a Low VRAM floor: a card under it stays on Low, the floors never pick Very Low.
    let mut floored = table.clone();
    floored.vram_floors.push(VramStep {
        min_mb: 4_000,
        tier: QualityTier::Low,
    });
    let small = AdapterProfile::named("NVIDIA GeForce GTX 1050", NVIDIA, Some(2_048));
    let d = detect(&small, &floored);
    assert_eq!(d.tier, QualityTier::Low, "{}", d.reason);
    assert!(d.caps.is_empty(), "{:?}", d.caps);
}

/// ADR-7882: the Intel N-series built-in graphics (the N150 laptop, Windows names it "Intel(R) Graphics") pick Very
/// Low by PCI device id; other Intel built-in graphics with the same name keep Low.
#[test]
fn intel_n_series_built_in_graphics_pick_very_low() {
    let table = table();
    let n150 = |device: u32, name: &str| {
        let mut a = AdapterProfile::named(name, INTEL, None);
        a.class = DeviceClass::Integrated;
        a.device = device;
        a
    };
    let d = detect(&n150(0x46d4, "Intel(R) Graphics"), &table);
    assert_eq!(
        (d.tier, d.rule.as_str(), d.evidence.as_str()),
        (QualityTier::VeryLow, "intel_n_series", "measured")
    );
    assert!(d.reason.contains("→ VeryLow"), "{}", d.reason);
    // The unmeasured siblings stay on Low until each id is measured (ADR-7979): the probe is off by
    // default, so a sibling that holds Low at 30 fps must not start on Very Low.
    for device in [0x46d0, 0x46d1, 0x46d2, 0x46d3] {
        let d = detect(&n150(device, "Intel(R) Graphics"), &table);
        assert_eq!(
            (d.tier, d.rule.as_str()),
            (QualityTier::Low, "intel_n_series_adl"),
            "{device:#x}"
        );
    }
    // Mesa's names on Linux: the measured id is Very Low, the name alone (no id) is the class row.
    assert_eq!(
        detect(&n150(0x46d4, "Intel(R) Graphics (ADL-N)"), &table).tier,
        QualityTier::VeryLow
    );
    assert_eq!(
        detect(&n150(0, "Intel(R) Graphics (ADL-N)"), &table).tier,
        QualityTier::Low
    );
    // Same name, another die (Meteor Lake / Lunar Lake report "Intel(R) Graphics" or "Intel(R) Arc(TM) Graphics"),
    // Iris Xe and UHD: Low as before.
    for (device, name) in [
        (0x7d55, "Intel(R) Graphics"),
        (0x9a49, "Intel(R) Iris(R) Xe Graphics"),
        (0x4680, "Intel(R) UHD Graphics 770"),
    ] {
        let d = detect(&n150(device, name), &table);
        assert_eq!(
            (d.tier, d.rule.as_str()),
            (QualityTier::Low, "intel_integrated"),
            "{name} {device:#x}"
        );
    }
    // A discrete Intel card with an N-series id is not this row.
    let mut discrete = n150(0x46d4, "Intel(R) Graphics");
    discrete.class = DeviceClass::Discrete;
    assert_ne!(detect(&discrete, &table).tier, QualityTier::VeryLow);
}

/// The texture streaming budget a frame runs with, by `begin_encode`'s two rules
/// (`frame/steps/begin.rs`): the discrete VRAM share, then the integrated-GPU limits when
/// `low_memory::integrated_limits_apply_sized_with` says so for this adapter and RAM size (no
/// `REGIA_INTEGRATED_LIMITS` switch, not Android, not a Deck).
fn frame_streaming(
    profile: &RenderProfile,
    adapter: &AdapterProfile,
    ram_mib: u64,
) -> crate::TextureStreaming {
    let mut streaming = profile.texture_streaming;
    streaming.budget_mib =
        streaming.vram_scaled_budget_mib(adapter.vram_mb, adapter.class == DeviceClass::Discrete);
    if crate::low_memory::integrated_limits_apply_sized_with(
        adapter.class,
        adapter.vendor,
        adapter.device,
        false,
        false,
        None,
        Some(ram_mib),
        adapter.vram_mb,
    ) {
        streaming = streaming.for_integrated_gpu(adapter.vram_mb, Some(ram_mib));
    }
    streaming
}

/// Very Low carries the integrated-GPU texture cap as its own column (ADR-7977 build B, ADR-8878):
/// 384 MiB with lazy first use in the profile, still 384 MiB on a 16 GB integrated laptop the
/// 15,000 MiB rule calls large (it would get Desktop Low's 1 GiB), grown to the VRAM share on a
/// discrete 8 GiB card as Desktop Low is; Low itself is unchanged.
#[test]
fn very_low_streams_under_the_integrated_cap_on_any_ram() {
    use crate::low_memory::INTEGRATED_STREAM_BUDGET_MIB;
    let very_low = QualityTier::VeryLow.profile();
    let low = QualityTier::Low.profile();
    assert_eq!(
        very_low.texture_streaming.budget_mib,
        INTEGRATED_STREAM_BUDGET_MIB
    );
    assert_eq!(INTEGRATED_STREAM_BUDGET_MIB, 384);
    assert!(!very_low.texture_streaming.eager, "lazy first use");
    assert!(very_low.texture_streaming.enabled);
    assert_eq!(
        low.texture_streaming.budget_mib, 1024,
        "Low keeps Desktop Low's 1 GiB"
    );
    assert_eq!(
        low.texture_streaming,
        RenderProfile::from_preset(RenderProfilePreset::DesktopLow).texture_streaming
    );

    // A 16 GB integrated laptop: "large" by the 15,000 MiB rule, so no integrated limits apply.
    let laptop = AdapterProfile {
        class: DeviceClass::Integrated,
        device: 0x46d4,
        ..AdapterProfile::named("Intel(R) Graphics", INTEL, Some(8192))
    };
    let ram = 16_384;
    assert!(!crate::low_memory::integrated_limits_apply_sized_with(
        laptop.class,
        laptop.vendor,
        laptop.device,
        false,
        false,
        None,
        Some(ram),
        laptop.vram_mb,
    ));
    assert_eq!(frame_streaming(&very_low, &laptop, ram).budget_mib, 384);
    assert!(!frame_streaming(&very_low, &laptop, ram).eager);
    assert_eq!(frame_streaming(&low, &laptop, ram).budget_mib, 1024);

    // A discrete 8 GiB card: the quarter-of-VRAM share, as Desktop Low gets.
    let card = AdapterProfile::named("NVIDIA GeForce GTX 1080", NVIDIA, Some(8192));
    assert_eq!(frame_streaming(&very_low, &card, ram).budget_mib, 2048);
    assert_eq!(frame_streaming(&low, &card, ram).budget_mib, 2048);

    // Applied live, the settings carry the column and still read as Desktop Low.
    let mut settings = RenderSettings::default();
    QualityTier::VeryLow.apply_for_gpu(&mut settings, Some(&laptop));
    assert_eq!(settings.quality().texture_streaming.budget_mib, 384);
    assert_eq!(settings.preset(), Some(RenderProfilePreset::DesktopLow));
    assert_eq!(QualityTier::of_settings(&settings), Some(QualityTier::VeryLow));
}
