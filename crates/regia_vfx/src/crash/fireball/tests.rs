// SPDX-License-Identifier: MPL-2.0
//! Fireball tests (ADR-1498): the seen size, timing and colour of the ball against the showcase's
//! X1 numbers (`<scratch>` § 3, 150–250 m at 1–1.5 s, black by 3–5 s,
//! rising 20–40 m/s into a toroid), its scaling with the yield, the footprint model against the
//! real fireball sheet, and the bounded fire light that removes the glare band.

use super::*;
use crate::crash::{GroundClass, crash_dust, measure_crash_effect};

const SHOW: CrashScale = CrashScale::SHOWCASE;

fn earth() -> VfxMedium {
    VfxMedium::earth()
}

fn at(def: &VfxSystemDef, t: f32) -> FireballFootprint {
    fireball_footprint(def, &[t])
        .first()
        .copied()
        .unwrap_or_else(|| panic!("{}: no footprint at {t} s", def.name))
}

/// The largest seen flame diameter over the first two seconds in 50 ms steps: `(time, diameter)`.
fn peak(def: &VfxSystemDef, until: f32) -> (f32, f32) {
    let times: Vec<f32> = (1..=(until / 0.05).round() as u32)
        .map(|i| i as f32 * 0.05)
        .collect();
    fireball_footprint(def, &times)
        .into_iter()
        .fold((0.0, 0.0), |best, f| {
            if f.flame_diameter > best.1 {
                (f.time, f.flame_diameter)
            } else {
                best
            }
        })
}

fn srgb_to_linear(b: u8) -> f32 {
    let c = f32::from(b) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// The footprint model's flame disc is the fireball sheet's own: the equivalent radius of the
/// texels that are opaque (alpha 0.5 and up) and hot enough to be flame in the billboard
/// shader (linear luma, its "heat", 0.21 and up: the middle of `SOOT_HEAT`), averaged over the
/// eight frames, is [`SHEET_FLAME_RADIUS`] of the tile width within 0.04.
#[test]
fn the_footprint_flame_disc_matches_the_fireball_sheet() {
    use crate::atlas::{FIREBALL_ANIM_FRAMES, FIREBALL_SHEET_WIDTH, FIREBALL_TILE, fireball_sheet_mips};
    let sheet = &fireball_sheet_mips()[0];
    let mut sum = 0.0f32;
    for frame in 0..FIREBALL_ANIM_FRAMES {
        let mut count = 0u32;
        for y in 0..FIREBALL_TILE {
            for x in 0..FIREBALL_TILE {
                let i = ((y * FIREBALL_SHEET_WIDTH + frame * FIREBALL_TILE + x) * 4) as usize;
                let heat = luma([
                    srgb_to_linear(sheet[i]),
                    srgb_to_linear(sheet[i + 1]),
                    srgb_to_linear(sheet[i + 2]),
                ]);
                if sheet[i + 3] >= 128 && heat >= 0.21 {
                    count += 1;
                }
            }
        }
        let radius = (count as f32 / std::f32::consts::PI).sqrt() / FIREBALL_TILE as f32;
        println!("sheet frame {frame}: flame radius {radius:.3} of the tile");
        sum += radius;
    }
    let mean = sum / FIREBALL_ANIM_FRAMES as f32;
    assert!(
        (mean - SHEET_FLAME_RADIUS).abs() <= 0.04,
        "the sheet's flame radius is {mean:.3} of the tile, the model uses {SHEET_FLAME_RADIUS}"
    );
}

/// X1 as seen (ADR-1498): at 250 t the projected flame is 150–250 m across (equivalent diameter)
/// at its largest, 1–1.5 s in, already 150 m at 1 s; soot covers a quarter of that area by 3 s
/// (soot-laden) and the flame is under a tenth of its peak area by 4.5 s (black by 3–5 s). On
/// both tiers, and through [`measure_crash_effect`].
#[test]
fn x1_the_seen_fireball_is_150_to_250_m_peaks_at_1_to_1_5_s_and_is_black_by_5_s() {
    for tier in CrashTier::ALL {
        let def = crash_fireball(SHOW, tier, &earth());
        let t = tier.name();
        let (when, d) = peak(&def, 2.5);
        println!("{t}: peak flame {d:.0} m at {when:.2} s");
        for f in fireball_footprint(&def, &[0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0, 3.0, 4.0, 4.5]) {
            println!(
                "{t} {:.2} s: flame {:.0} m, soot {:.0} m², {:.0} x {:.0} m, centre {:.0} m",
                f.time, f.flame_diameter, f.soot_area, f.width, f.height, f.centre_height
            );
        }
        assert!((150.0..=250.0).contains(&d), "{t}: peak flame diameter {d}");
        assert!((1.0..=1.5).contains(&when), "{t}: peak at {when} s");
        let one = at(&def, 1.0);
        assert!(
            one.flame_diameter >= 150.0,
            "{t}: {} m at 1 s",
            one.flame_diameter
        );
        assert!(one.width <= 320.0, "{t}: {} m wide at 1 s", one.width);
        let peak_area = std::f32::consts::PI * 0.25 * d * d;
        let three = at(&def, 3.0);
        assert!(
            three.soot_area >= 0.25 * peak_area,
            "{t}: soot {} m² at 3 s",
            three.soot_area
        );
        let late = at(&def, 4.5);
        assert!(
            late.flame_area <= 0.1 * peak_area,
            "{t}: flame {} m² at 4.5 s",
            late.flame_area
        );
        let m = measure_crash_effect(&def);
        let r = m.fireball_peak_radius.unwrap_or_default();
        assert!((75.0..=125.0).contains(&r), "{t}: measured radius {r}");
    }
}

/// The yield sizes the seen ball: from a 1 t vehicle crash to 1 kt the peak equivalent diameter
/// is `3.5 W^(1/3)` (twice [`CrashScale::fireball_radius`]) within −25 % / +15 %, and grows.
#[test]
fn the_seen_fireball_scales_with_the_yield() {
    let mut last = 0.0;
    for tonnes in [1.0, 10.0, 250.0, 1_000.0] {
        let scale = CrashScale::from_tonnes(tonnes);
        let def = crash_fireball(scale, CrashTier::Full, &earth());
        let (when, d) = peak(&def, 2.0 * scale.time_scale());
        let k = d / (2.0 * scale.fireball_radius());
        println!(
            "{}: peak flame {d:.1} m ({k:.2} of 3.5 W^1/3) at {when:.2} s",
            scale.tag()
        );
        assert!((0.75..=1.15).contains(&k), "{}: {k}", scale.tag());
        assert!(d > last, "{}: grows", scale.tag());
        last = d;
    }
}

/// The ball rises buoyantly (20–40 m/s between `0.5 τ` and `τ`) and its soot rolls into a
/// toroid: at 4 and 5 s the soot puffs sit on a ring (median distance from their axis at least
/// half a fireball radius, the 25th percentile at least 55 % of it, so the axis is thin) and the
/// sooty mass is wider than tall.
#[test]
fn the_fireball_rises_into_a_soot_toroid() {
    let big_r = SHOW.fireball_radius();
    for tier in CrashTier::ALL {
        let def = crash_fireball(SHOW, tier, &earth());
        let t = tier.name();
        let rise = measure_crash_effect(&def).fireball_rise.unwrap_or_default();
        assert!((20.0..=40.0).contains(&rise), "{t}: rise {rise} m/s");
        let frames = fireball_footprint(&def, &[1.0, 4.0, 5.0]);
        for f in &frames[1..] {
            println!(
                "{t} {:.1} s: ring {:.0} m, hole {:.2}, aspect {:.2}, top {:.0} m",
                f.time, f.soot_ring_radius, f.soot_hole, f.soot_aspect, f.top
            );
            assert!(
                f.soot_ring_radius >= 0.5 * big_r,
                "{t} {}: ring {}",
                f.time,
                f.soot_ring_radius
            );
            assert!(f.soot_hole >= 0.55, "{t} {}: hole {}", f.time, f.soot_hole);
            assert!(f.soot_aspect >= 1.0, "{t} {}: aspect {}", f.time, f.soot_aspect);
        }
        assert!(frames[2].top > frames[0].top, "{t}: the mass rises");
    }
}

/// The flash is a smooth additive glow (no fireball-sheet card, so no flame flicks around a
/// flat sprite) and both lights of the ball are sphere sources of the ball's radius.
#[test]
fn the_flash_is_a_smooth_glow_and_the_lights_are_the_ball() {
    for scale in [CrashScale::VEHICLE, SHOW, CrashScale::LARGEST] {
        let def = crash_fireball(scale, CrashTier::Full, &earth());
        let flash = def.emitters.iter().find(|e| e.name == "flash").expect("flash");
        assert_eq!(flash.render.sprite, SpriteId::SoftDisc);
        assert_eq!(flash.render.blend, BlendMode::Additive);
        assert!(flash.render.flipbook.is_none());
        let big_r = scale.fireball_radius();
        for e in def.emitters.iter().filter(|e| e.render.light.is_some()) {
            let radius = e.render.light.as_ref().map_or(0.0, |l| l.radius);
            let source = crate::runtime::particle_light_source_radius(radius).unwrap_or_default();
            assert!(
                (source - big_r).abs() <= 1e-3 * big_r,
                "{} {}: source {source} for R {big_r}",
                def.name,
                e.name
            );
        }
    }
}

/// [`fire_light_falloff`] keeps a punctual light (a ground fire, 30 m) bit for bit, and bounds a
/// fireball light under the ball (at most `1 / s²`) while matching the inverse square far out.
#[test]
fn the_fire_light_falloff_is_punctual_for_fires_and_bounded_for_the_ball() {
    for d in [0.0f32, 0.5, 3.0, 12.0, 29.0] {
        let ratio = d / 30.0;
        let window = (1.0 - ratio * ratio * ratio * ratio).clamp(0.0, 1.0);
        assert_eq!(
            fire_light_falloff(d, 30.0).to_bits(),
            (window * window / (1.0 + d * d)).to_bits(),
            "{d}"
        );
    }
    let big_r = SHOW.fireball_radius();
    let reach = FLASH_RADIUS_PER_SOURCE * big_r;
    for i in 0..=100 {
        let d = i as f32 * 0.1 * big_r;
        assert!(
            fire_light_falloff(d, reach) <= 1.0 / (big_r * big_r) + 1e-12,
            "{d}"
        );
    }
    let far = 10.0 * big_r;
    let inverse_square = 1.0 / (far * far);
    assert!((fire_light_falloff(far, reach) / inverse_square - 1.0).abs() < 0.02);
}

/// The glare band (ADR-1498): at daylight exposure (EV 15) the body's light lights a puff under
/// the ball (the dust skirt) at most to display white, where the punctual term at the old spawn
/// height (0.35 R) put it several times past white.
#[test]
fn a_puff_under_the_ball_is_not_lit_past_white() {
    let def = crash_fireball(SHOW, CrashTier::Full, &earth());
    let light = def
        .emitters
        .iter()
        .find(|e| e.name == "fireball")
        .and_then(|e| e.render.light.clone())
        .expect("body light");
    let exposure = regia_render::exposure_from_ev100(15.0);
    let pi = std::f32::consts::PI;
    // `VfxGpu::set_sun`'s mapping of the fire light into display units.
    let fire_scale = crate::runtime::PARTICLE_LIGHT_LUMENS_PER_UNIT / (4.0 * pi * pi) * exposure;
    let fire = light.intensity * fire_scale;
    let big_r = SHOW.fireball_radius();
    for i in 0..=60 {
        let d = i as f32 * 0.05 * big_r;
        let lit = fire * fire_light_falloff(d, light.radius);
        assert!(lit <= 1.0, "{lit} at {d} m");
    }
    let old = 0.35 * big_r;
    assert!(fire / (1.0 + old * old) > 2.0, "the old term clipped the skirt");
}

/// The billboard shader's fire-light knee uses the renderer's flash constants.
#[test]
fn the_billboard_shader_mirrors_the_flash_constants() {
    let src = crate::gpu::shaders::BILLBOARD;
    assert!(src.contains(&format!(
        "const FLASH_MIN_RADIUS_M: f32 = {FLASH_MIN_RADIUS_M:?};"
    )));
    assert!(src.contains(&format!(
        "const FLASH_RADIUS_PER_SOURCE: f32 = {FLASH_RADIUS_PER_SOURCE:?};"
    )));
    assert!(src.contains("fire_source_sq(frame.fire_pos.w)"));
}

/// The footprint is a pure function of the definition (seeded probe, fixed raster), and leaves
/// out what is not the ball (a dust layer has no flame).
#[test]
fn the_footprint_is_deterministic_and_only_fire_is_flame() {
    let def = crash_fireball(SHOW, CrashTier::Lite, &earth());
    let times = [0.5, 1.25, 3.0];
    assert_eq!(fireball_footprint(&def, &times), fireball_footprint(&def, &times));
    let dust = crash_dust(GroundClass::Snow, SHOW, CrashTier::Lite, &earth());
    for f in fireball_footprint(&dust, &times) {
        assert!(f.flame_area <= 0.0, "{} s: {}", f.time, f.flame_area);
    }
}
