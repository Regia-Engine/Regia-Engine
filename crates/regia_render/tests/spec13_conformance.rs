// SPDX-License-Identifier: MPL-2.0
//! Spec 13 (and spec 12 § 5b) conformance checks for values the specs state that had no test of
//! their own (lane spec-04-12-assets, ADR-5611). CPU only: no device is created.

use regia_render::forward_lite::ForwardLite;
use regia_render::{
    AntiAliasing, BudgetTier, ColorLut, HdrTarget, MemoryBudget, PostSettings, RenderProfile,
    RenderProfilePreset, ShadowFilter, SpatialUpscaler, SsaoMode, TextureStreaming, TonemapOperator,
    UploadQueue,
};

/// One row of the spec 13 § 3 profile table, per preset.
struct Row {
    preset: RenderProfilePreset,
    hdr: HdrTarget,
    scale: f32,
    aa: AntiAliasing,
    upscaler: SpatialUpscaler,
    prepass: bool,
    cascades: u32,
    cascade_res: u32,
    atlas: u32,
    atlas_lights: u32,
    filter: ShadowFilter,
    lite: ForwardLite,
    ssao: SsaoMode,
    bloom: bool,
    auto_exposure: bool,
    probes: u32,
    lights_per_cluster: u32,
}

/// Spec 13 § 3: every cell of the profile table (Mobile Low, Mobile High, Desktop Low, Desktop
/// High, Desktop Ultra), plus Mobile High's 4 bloom mips and the "no preset uses MSAA" rule.
#[test]
fn every_preset_matches_the_spec_13_profile_table() {
    use RenderProfilePreset as P;
    let rows = [
        Row {
            preset: P::MobileLow,
            hdr: HdrTarget::Rg11B10,
            scale: 0.7,
            aa: AntiAliasing::None,
            upscaler: SpatialUpscaler::Sgsr,
            prepass: false,
            cascades: 1,
            cascade_res: 1024,
            atlas: 0,
            atlas_lights: 0,
            filter: ShadowFilter::Hardware2x2,
            lite: ForwardLite::Low,
            ssao: SsaoMode::Off,
            bloom: false,
            auto_exposure: false,
            probes: 1,
            lights_per_cluster: 16,
        },
        Row {
            preset: P::MobileHigh,
            hdr: HdrTarget::Rg11B10,
            scale: 0.85,
            aa: AntiAliasing::Smaa,
            upscaler: SpatialUpscaler::Sgsr,
            prepass: false,
            cascades: 2,
            cascade_res: 2048,
            atlas: 4096,
            atlas_lights: 4,
            filter: ShadowFilter::Pcf3x3,
            lite: ForwardLite::High,
            ssao: SsaoMode::Off,
            bloom: true,
            auto_exposure: true,
            probes: 2,
            lights_per_cluster: 32,
        },
        Row {
            preset: P::DesktopLow,
            hdr: HdrTarget::Rgba16f,
            scale: 0.75,
            aa: AntiAliasing::Fxaa,
            upscaler: SpatialUpscaler::Bilinear,
            prepass: true,
            cascades: 2,
            cascade_res: 2048,
            atlas: 4096,
            atlas_lights: 8,
            filter: ShadowFilter::Pcf,
            lite: ForwardLite::Off,
            ssao: SsaoMode::HalfRes,
            bloom: true,
            auto_exposure: true,
            probes: 2,
            lights_per_cluster: 64,
        },
        Row {
            preset: P::DesktopHigh,
            hdr: HdrTarget::Rgba16f,
            scale: 1.0,
            aa: AntiAliasing::Taa,
            // The table's "—": a TAA preset never takes the spatial path, the field keeps bilinear.
            upscaler: SpatialUpscaler::Bilinear,
            prepass: true,
            cascades: 4,
            cascade_res: 2048,
            atlas: 8192,
            atlas_lights: 32,
            filter: ShadowFilter::PcssLite,
            lite: ForwardLite::Off,
            ssao: SsaoMode::GtaoHalfRes,
            bloom: true,
            auto_exposure: true,
            probes: 4,
            lights_per_cluster: 128,
        },
        Row {
            preset: P::DesktopUltra,
            hdr: HdrTarget::Rgba16f,
            scale: 1.0,
            aa: AntiAliasing::Taa,
            upscaler: SpatialUpscaler::Bilinear,
            prepass: true,
            cascades: 4,
            cascade_res: 4096,
            atlas: 8192,
            atlas_lights: 64,
            filter: ShadowFilter::PcssLite,
            lite: ForwardLite::Off,
            ssao: SsaoMode::GtaoFull,
            bloom: true,
            auto_exposure: true,
            probes: 4,
            lights_per_cluster: 256,
        },
    ];
    for row in &rows {
        let p = RenderProfile::from_preset(row.preset);
        let name = format!("{:?}", row.preset);
        assert_eq!(p.hdr_target, row.hdr, "{name} HDR target");
        assert!(
            (p.render_scale - row.scale).abs() < 1.0e-6,
            "{name} render scale {}",
            p.render_scale
        );
        assert_eq!(p.anti_aliasing, row.aa, "{name} AA");
        assert_eq!(p.spatial_upscaler, row.upscaler, "{name} upscale");
        assert_eq!(p.depth_prepass, row.prepass, "{name} depth prepass");
        assert_eq!(p.csm_cascade_count, row.cascades, "{name} cascades");
        assert_eq!(p.csm_resolution, row.cascade_res, "{name} cascade resolution");
        assert_eq!(p.local_shadow_atlas_size, row.atlas, "{name} local shadow atlas");
        assert_eq!(
            p.local_shadow_max_lights, row.atlas_lights,
            "{name} local shadow lights"
        );
        assert_eq!(p.shadow_filter, row.filter, "{name} shadow filter");
        assert_eq!(
            p.forward_lite, row.lite,
            "{name} forward shader on tile GPUs (ADR-1874)"
        );
        assert_eq!(p.ssao, row.ssao, "{name} SSAO");
        assert_eq!(p.bloom, row.bloom, "{name} bloom");
        assert_eq!(p.auto_exposure, row.auto_exposure, "{name} auto exposure");
        assert_eq!(p.max_reflection_probes, row.probes, "{name} reflection probes");
        assert_eq!(
            p.max_lights_per_cluster, row.lights_per_cluster,
            "{name} lights per cluster"
        );
        assert_ne!(
            p.anti_aliasing,
            AntiAliasing::Msaa4x,
            "{name}: no preset uses MSAA"
        );
    }
    assert_eq!(
        RenderProfile::from_preset(P::MobileHigh).bloom_mip_count,
        4,
        "Mobile High blooms over 4 mips"
    );
}

/// Spec 13 § 3 texture memory table: resident cap and budget of the streamed material maps
/// per preset (ADR-130; 0 px = the authored size).
#[test]
fn texture_streaming_presets_match_the_spec_13_table() {
    use RenderProfilePreset as P;
    for (preset, cap_px, budget_mib) in [
        (P::MobileLow, 1024, 256),
        (P::MobileHigh, 2048, 512),
        (P::Deck, 2048, 1024),
        (P::DesktopLow, 2048, 1024),
        (P::DesktopHigh, 4096, 2048),
        (P::DesktopUltra, 0, 4096),
    ] {
        let streaming = TextureStreaming::for_preset(preset);
        assert!(streaming.enabled, "{preset:?} streams");
        assert_eq!(streaming.max_px, cap_px, "{preset:?} resident cap");
        assert_eq!(streaming.budget_mib, budget_mib, "{preset:?} budget");
        assert_eq!(
            RenderProfile::from_preset(preset).texture_streaming,
            streaming,
            "{preset:?} profile carries its streaming policy"
        );
    }
}

/// Spec 13 § 9: motion blur and depth of field are allowed on Desktop High, Ultra and the Deck,
/// and on no other preset.
#[test]
fn lens_effects_are_allowed_on_desktop_high_ultra_and_deck_only() {
    use RenderProfilePreset as P;
    for preset in [
        P::MobileLow,
        P::MobileHigh,
        P::DesktopLow,
        P::DesktopHigh,
        P::DesktopUltra,
        P::Deck,
    ] {
        let p = RenderProfile::from_preset(preset);
        let allowed = matches!(preset, P::DesktopHigh | P::DesktopUltra | P::Deck);
        assert_eq!(p.motion_blur, allowed, "{preset:?} motion blur");
        assert_eq!(p.depth_of_field, allowed, "{preset:?} depth of field");
    }
}

/// Spec 13 § 5 item 12: AgX is the default tonemap, film grain is off by default, and a 3D LUT
/// has an edge of at most 64 (a larger cube is refused).
#[test]
fn post_defaults_and_the_lut_edge_cap() {
    let post = PostSettings::default();
    assert_eq!(post.tonemap, TonemapOperator::AgX);
    assert_eq!(post.film_grain.to_bits(), 0.0f32.to_bits(), "film grain off");
    assert_eq!(ColorLut::MAX_SIZE, 64);
    assert!(ColorLut::from_entries(64, vec![[0.0; 3]; 64 * 64 * 64]).is_some());
    assert!(ColorLut::from_entries(65, vec![[0.0; 3]; 65 * 65 * 65]).is_none());
    assert!(ColorLut::parse_cube("LUT_3D_SIZE 65\n").is_err());
    // The strip's shape is checked before its pixels are read.
    assert!(ColorLut::from_strip_rgba8(65 * 65, 65, &[]).is_err());
}

/// Spec 12 § 5b: the upload queue's defaults are a 1.5 ms frame budget, at least 256 KiB a
/// frame and no single write over 1 MiB.
#[test]
fn upload_queue_defaults_match_spec_12() {
    let queue = UploadQueue::default();
    assert_eq!(queue.budget_us, 1500);
    assert_eq!(queue.min_frame_bytes, 256 * 1024);
    assert_eq!(queue.max_call_bytes, 1024 * 1024);
}

/// Spec 12 § 5b: the memory budget per tier (minimum: 6.5 GiB VRAM / 10 GiB RSS; Deck 3 / 5 GiB;
/// Desktop High 10 / 16 GiB).
#[test]
fn memory_budget_tiers_match_spec_12() {
    const GIB: u64 = 1 << 30;
    let minimum = MemoryBudget::for_tier(BudgetTier::Minimum);
    assert_eq!((minimum.vram, minimum.ram), (13 * GIB / 2, 10 * GIB));
    let deck = MemoryBudget::for_tier(BudgetTier::Deck);
    assert_eq!((deck.vram, deck.ram), (3 * GIB, 5 * GIB));
    let high = MemoryBudget::for_tier(BudgetTier::DesktopHigh);
    assert_eq!((high.vram, high.ram), (10 * GIB, 16 * GIB));
}
