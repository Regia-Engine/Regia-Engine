// SPDX-License-Identifier: MPL-2.0
//! Crash-effect tests: the showcase acceptance numbers (`<scratch>` § 3
//! X1–X3 and X6, § 3a/3b: a 0.2–0.5 kt surface explosion), measured with the CPU probe
//! ([`crate::probe`]) at [`CrashScale::SHOWCASE`] on both tiers and every ground class, the
//! scale range from a vehicle crash to 1 kt, the clock and its hooks, and the distance LOD.

use super::*;

/// Pinned FNV of the `crash_fire_sites(SHOW, 11)` offset bits (deterministic sin/cos).
// The offsets are world positions spawned into the scene, built on the deterministic
// sin/cos pair since the vfx1 determinism audit (finding 6); the radii feeding them come
// from `det_cbrt` since fixrev-22-k (Muse review finding 2). On glibc the deterministic
// kernels agree with libm bit for bit at every scale in use, so this pin is unchanged; on a
// platform whose libm `cbrt` differs by an ulp it now still holds.
const FIRE_SITE_OFFSET_BITS_HASH: u64 = 11_706_560_716_580_741_883;

use crate::format::SpriteId;
use crate::probe::{ProbeSettings, ProbeSnapshot, probe_effect};
use crate::scene::VfxLibrary;

const SHOW: CrashScale = CrashScale::SHOWCASE;

/// The showcase impact site and shot 4's camera (PLAN.md test site, +Z north): 3.3 km.
const SITE: Vec3 = Vec3::new(700.0, 105.7, 5.0);
const SHOT4: Vec3 = Vec3::new(-1752.0, 353.2, -2203.0);

fn earth() -> VfxMedium {
    VfxMedium::earth()
}

fn at(def: &VfxSystemDef, times: &[f32]) -> Vec<ProbeSnapshot> {
    probe_effect(def, Vec3::ZERO, Vec3::Y, ProbeSettings::default(), times)
}

/// Asserts `value` is measured and inside `lo..=hi`; returns it.
fn band(what: &str, value: Option<f32>, lo: f32, hi: f32) -> f32 {
    let v = value.unwrap_or_else(|| panic!("{what}: not measured"));
    assert!((lo..=hi).contains(&v), "{what}: {v} not in {lo}..={hi}");
    v
}

#[test]
fn scale_quantises_names_and_round_trips() {
    assert_eq!(CrashScale::from_tonnes(250.0), SHOW);
    assert_eq!(SHOW.tag(), "250t");
    assert_eq!(CrashScale::from_tonnes(0.25).tag(), "0.25t");
    assert_eq!(CrashScale::from_tonnes(1.0), CrashScale::VEHICLE);
    assert_eq!(CrashScale::from_tonnes(123.456).tag(), "123t");
    assert_eq!(CrashScale::from_tonnes(0.012_345).tag(), "0.05t", "clamped up");
    assert_eq!(
        CrashScale::from_tonnes(5_000.0),
        CrashScale::LARGEST,
        "clamped down"
    );
    assert_eq!(CrashScale::LARGEST.tag(), "1000t");
    assert_eq!(CrashScale::from_tonnes(f32::NAN), CrashScale::VEHICLE);
    assert_eq!(CrashScale::from_effect_name("crash_fireball@250t"), Some(SHOW));
    assert_eq!(
        CrashScale::from_effect_name("crash_dust_snow@0.5t").map(CrashScale::tonnes),
        Some(0.5)
    );
    assert_eq!(
        CrashScale::from_effect_name("crash_fireball@250.0t"),
        None,
        "not canonical"
    );
    assert_eq!(CrashScale::from_effect_name("crash_fireball"), None);
    for i in 0..200 {
        let s = CrashScale::from_tonnes(0.05 * 1.05f32.powi(i));
        assert_eq!(
            CrashScale::from_tonnes(s.tonnes()),
            s,
            "idempotent at {}",
            s.tonnes()
        );
        assert_eq!(CrashScale::from_effect_name(&fireball_effect(s)), Some(s));
    }
}

/// § 3a/3b: cube-root scaling at 0.25 kt gives the plan's sizes.
#[test]
fn showcase_scale_numbers_follow_cube_root_scaling() {
    band(
        "fireball diameter",
        Some(2.0 * SHOW.fireball_radius()),
        150.0,
        250.0,
    );
    let crater = SHOW.crater();
    band("crater diameter", Some(crater.diameter), 40.0, 70.0);
    band("crater depth", Some(crater.depth), 10.0, 20.0);
    band("crater rim", Some(crater.rim_height), 3.0, 5.0);
    band("ejecta blanket", Some(crater.ejecta_blanket_radius), 120.0, 180.0);
    band("scorch radius", Some(crater.scorch_radius), 250.0, 350.0);
    band("fire count", Some(SHOW.fire_count() as f32), 30.0, 80.0);
    band("column height", Some(SHOW.column_height()), 800.0, 1_500.0);
    band("blast reach", Some(SHOW.blast_reach()), 1_800.0, 2_200.0);
    band("lift reach", Some(SHOW.lift_reach()), 300.0, 500.0);
    band("shell radius", Some(SHOW.shell_radius()), 150.0, 300.0);
    // Cube root: eight times the yield doubles every length.
    for t in [0.1, 1.0, 10.0, 100.0] {
        let (a, b) = (CrashScale::from_tonnes(t), CrashScale::from_tonnes(8.0 * t));
        assert!((b.fireball_radius() / a.fireball_radius() - 2.0).abs() < 1e-3);
        assert!((b.crater().diameter / a.crater().diameter - 2.0).abs() < 1e-3);
    }
    // Far-field shake falls with distance: strong at 500 m, mild at shot 4's 3.3 km.
    let far = SHOW.blast_shake(SITE.distance(SHOT4));
    assert!(SHOW.blast_shake(500.0) > far && far > 0.05 && far < 0.5, "{far}");
}

#[test]
fn every_crash_effect_validates_round_trips_and_is_v5() {
    for tonnes in [0.05, 1.0, 250.0, 1_000.0] {
        let scale = CrashScale::from_tonnes(tonnes);
        for tier in CrashTier::ALL {
            let defs = crash_effects(scale, tier, &earth());
            assert_eq!(
                defs.len(),
                16,
                "fireball, shell, column, 3 × (dust, ejecta, contact), 3 fires, gust puff"
            );
            let mut names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
            names.sort_unstable();
            names.dedup();
            assert_eq!(names.len(), 16, "unique names");
            for def in &defs {
                assert_eq!(def.version, VFX_FORMAT_VERSION);
                def.validate()
                    .unwrap_or_else(|e| panic!("{} validates: {e}", def.name));
                let text = def.to_ron().unwrap_or_else(|e| panic!("{}: {e}", def.name));
                let back = VfxSystemDef::from_ron(&text).unwrap_or_else(|e| panic!("{}: {e}", def.name));
                assert_eq!(def, &back, "{} round-trips", def.name);
                assert_eq!(
                    crash_effect(&def.name, tier, &earth()).as_ref(),
                    Some(def),
                    "lookup by name"
                );
                for e in &def.emitters {
                    if let SpawnMode::Burst(n) = e.spawn.mode {
                        assert!(
                            n <= def.system.budget,
                            "{}/{}: burst over budget",
                            def.name,
                            e.name
                        );
                    }
                }
            }
        }
    }
    assert!(crash_effect("explosion", CrashTier::Full, &earth()).is_none());
    assert!(crash_effect("crash_fireball", CrashTier::Full, &earth()).is_none());
    assert!(crash_effect("crash_fireball@250.0t", CrashTier::Full, &earth()).is_none());
}

#[test]
fn tiers_follow_the_profile_and_lite_is_cheaper() {
    assert_eq!(
        CrashTier::for_profile(&VfxProfile::desktop_high()),
        CrashTier::Full
    );
    assert_eq!(
        CrashTier::for_profile(&VfxProfile::desktop_ultra()),
        CrashTier::Full
    );
    assert_eq!(
        CrashTier::for_profile(&VfxProfile::mobile_high()),
        CrashTier::Lite
    );
    for tier in CrashTier::ALL {
        assert_eq!(CrashTier::from_name(tier.name()), Some(tier));
    }
    for ground in GroundClass::ALL {
        assert_eq!(GroundClass::from_name(ground.name()), Some(ground));
    }
    assert_eq!(CrashTier::from_name("ultra"), None);
    let peak = |def: &VfxSystemDef, times: &[f32]| {
        at(def, times)
            .iter()
            .map(|s| s.emitters.iter().map(|e| e.live).sum::<u32>())
            .max()
            .unwrap_or(0)
    };
    let full = crash_effects(SHOW, CrashTier::Full, &earth());
    let lite = crash_effects(SHOW, CrashTier::Lite, &earth());
    for (f, l) in full.iter().zip(&lite) {
        assert_eq!(f.name, l.name);
        assert!(l.emitters.len() <= f.emitters.len(), "{}", f.name);
        assert!(
            l.emitters.iter().all(|e| e.render.heat_haze == 0.0),
            "{}: no haze on lite (it needs a scene copy)",
            l.name
        );
        if f.name.starts_with("crash_ground_fire") {
            continue;
        }
        let times: &[f32] = if f.name.starts_with(CRASH_COLUMN) {
            &[30.0, 60.0]
        } else {
            &[0.5, 1.0, 2.0, 4.0]
        };
        let (pf, pl) = (peak(f, times), peak(l, times));
        assert!(
            pl * 2 <= pf + 8,
            "{}: lite peaks at {pl} live, full at {pf}",
            f.name
        );
    }
}

/// § 3b, no tropes: no camera-facing ring, no flame trails or glow on debris, and only the
/// fire layers carry a light.
#[test]
fn no_camera_facing_ring_and_debris_does_not_glow() {
    for tier in CrashTier::ALL {
        for def in crash_effects(SHOW, tier, &earth()) {
            for e in &def.emitters {
                let what = format!("{}/{}", def.name, e.name);
                assert_ne!(e.render.sprite, SpriteId::Ring, "{what}");
                assert_ne!(
                    e.render.mode,
                    RenderMode::Stretched,
                    "{what}: no streaks or trails"
                );
                if e.render.mode == RenderMode::Mesh {
                    assert!(e.render.light.is_none(), "{what}");
                    assert_ne!(e.render.blend, BlendMode::Additive, "{what}");
                    assert!(e.render.emissive <= 1.0, "{what}: debris does not glow");
                }
                if e.render.light.is_some() {
                    assert!(
                        ["flash", "fireball", "wreck_fire", "flames"].contains(&e.name.as_str()),
                        "{what}: only fire lights the scene"
                    );
                }
            }
        }
    }
}

/// X1: the flash lasts 50–200 ms and its light reaches the land 2–3 km away.
#[test]
fn x1_flash_lasts_50_to_200_ms_and_lights_kilometres() {
    for scale in [CrashScale::VEHICLE, SHOW] {
        let def = crash_fireball(scale, CrashTier::Full, &earth());
        band("flash end", measure_crash_effect(&def).flash_end, 0.05, 0.2);
    }
    let def = crash_fireball(SHOW, CrashTier::Lite, &earth());
    let light = def
        .emitters
        .iter()
        .find(|e| e.name == "flash")
        .and_then(|e| e.render.light.clone())
        .expect("flash light");
    assert!(light.radius >= 3_000.0, "lights 3 km: {}", light.radius);
    // Lumens at 2.5 km (inverse square only): brighter than an overcast day (~1 klx).
    let lumens = light.intensity * crate::runtime::PARTICLE_LIGHT_LUMENS_PER_UNIT;
    let lux = lumens / (4.0 * std::f32::consts::PI * 2_500.0 * 2_500.0);
    assert!(lux > 1_000.0, "{lux} lx at 2.5 km");
}

/// X1: 150–250 m across at its largest, 1–1.5 s in, rising at 20–40 m/s, soot-black by
/// 3–5 s.
#[test]
fn x1_fireball_is_150_to_250_m_across_peaks_at_1_to_1_5_s_and_turns_sooty() {
    for tier in CrashTier::ALL {
        let m = measure_crash_effect(&crash_fireball(SHOW, tier, &earth()));
        let t = tier.name();
        band(&format!("{t} peak radius"), m.fireball_peak_radius, 75.0, 125.0);
        band(&format!("{t} peak time"), m.fireball_peak_time, 1.0, 1.5);
        band(&format!("{t} fire gone"), m.fire_gone_at, 3.0, 5.5);
        band(&format!("{t} rise"), m.fireball_rise, 20.0, 40.0);
        band(&format!("{t} soot time"), m.soot_time, 3.0, 5.0);
        band(&format!("{t} soot luma"), m.soot_luma, 0.0, 0.05);
        band(&format!("{t} soot alpha"), m.soot_alpha, 0.5, 1.0);
    }
}

#[test]
fn x1_fire_stays_in_the_family_no_pink_core() {
    use crate::curves::sample_color;
    use crate::format::parse_color;
    use crate::quality::in_fire_family;

    let ev15 = regia_render::exposure_from_ev100(15.0);
    let stops: Vec<[f32; 3]> = crate::atlas::fireball_ramp_stops().collect();
    let def = crash_fireball(SHOW, CrashTier::Full, &earth());
    for name in ["fireball", "billows"] {
        let fire = def.emitters.iter().find(|e| e.name == name).expect(name);
        assert_eq!(fire.render.sprite, SpriteId::FireballAnim);
        let init = parse_color(&fire.init.color[0].color).expect("init");
        for step in 0..=8 {
            let t = step as f32 / 8.0;
            let over = sample_color(&fire.update.color_over_life, t);
            let tint = [init[0] * over[0], init[1] * over[1], init[2] * over[2]];
            let spread = tint[0].max(tint[1]).max(tint[2]) - tint[0].min(tint[1]).min(tint[2]);
            assert!(
                spread < 0.02,
                "{name}: the tint must stay neutral, t={t} {tint:?}"
            );
            for stop in &stops {
                let rgb = [stop[0] * tint[0], stop[1] * tint[1], stop[2] * tint[2]];
                assert!(in_fire_family(rgb), "{name} t={t} {rgb:?}");
                let hdr = rgb.map(|c| c * fire.render.emissive);
                let back = crate::quality::restore_exposed(hdr, ev15);
                assert!(
                    in_fire_family(regia_render::tonemap_agx(back)),
                    "{name}: AgX at t={t} left the family: {back:?}"
                );
            }
        }
    }
    let mut flames: Vec<EmitterDef> = crash_column(SHOW, CrashTier::Full, &earth())
        .emitters
        .into_iter()
        .filter(|e| e.name == "wreck_fire")
        .collect();
    for size in FireSize::ALL {
        flames.extend(
            crash_ground_fire(size, CrashTier::Full, &earth())
                .emitters
                .into_iter()
                .filter(|e| e.name == "flames"),
        );
    }
    assert_eq!(flames.len(), 4);
    for e in &flames {
        for key in &e.update.color_over_life {
            let c = parse_color(&key.color).expect("tint");
            assert!(
                c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2]) < 0.02,
                "neutral flame tint"
            );
        }
    }
}

/// X1 (optional): the condensation shell is 150–300 m out and gone within 1 s.
#[test]
fn x1_condensation_shell_is_150_to_300_m_and_gone_within_1_s() {
    for tier in CrashTier::ALL {
        let m = measure_crash_effect(&crash_shell(SHOW, tier));
        band(
            &format!("{} shell radius", tier.name()),
            m.shell_radius,
            150.0,
            300.0,
        );
        band(&format!("{} shell gone", tier.name()), m.shell_gone_at, 0.2, 1.0);
    }
    assert!(!CrashBurst::default().shell, "humid air only: off by default");
}

/// X2: the blast ring runs at 343 m/s ± 10 % beyond 150 m and reaches 2 km at about 6 s.
#[test]
fn x2_blast_ring_runs_at_343_m_s_to_2_km() {
    let m = measure_crash_effect(&crash_dust(GroundClass::Snow, SHOW, CrashTier::Full, &earth()));
    band("front speed", m.front_speed, 0.9 * 343.0, 1.1 * 343.0);
    band("front error beyond 150 m", m.front_error, 0.0, 0.1);
    band("front reach", m.front_reach, 1_800.0, 2_300.0);
    let lite = measure_crash_effect(&crash_dust(GroundClass::Snow, SHOW, CrashTier::Lite, &earth()));
    band("lite front speed", lite.front_speed, 0.9 * 343.0, 1.1 * 343.0);
    band("lite front error beyond 150 m", lite.front_error, 0.0, 0.1);
    // Nothing in the crash set draws from the unsorted haze list in bulk (byte-stable frames).
    for def in crash_effects(SHOW, CrashTier::Full, &earth()) {
        let haze = def
            .emitters
            .iter()
            .filter(|e| e.render.heat_haze > 0.0)
            .map(|e| match e.spawn.mode {
                SpawnMode::Burst(n) => n,
                _ => u32::MAX,
            })
            .sum::<u32>();
        assert!(haze <= 3, "{}: {haze} haze cards", def.name);
    }
}

/// X2: the blast lifts snow and dust along its path out to 300–500 m, never ahead of the
/// front.
#[test]
fn x2_lifted_cover_reaches_300_to_500_m_behind_the_front() {
    for tier in CrashTier::ALL {
        for ground in GroundClass::ALL {
            let what = format!("{} {}", tier.name(), ground.name());
            let m = measure_crash_effect(&crash_dust(ground, SHOW, tier, &earth()));
            band(&format!("{what} lift reach"), m.lift_reach, 300.0, 500.0);
            assert_eq!(
                m.lift_leads_front,
                Some(false),
                "{what}: lifted cover leads the front"
            );
        }
    }
}

/// X2: the base surge is 400–700 m out and 50–100 m high about 15 s after a 0.25 kt blast.
#[test]
fn x2_base_surge_is_400_to_700_m_out_and_50_to_100_m_high_by_15_s() {
    for tier in CrashTier::ALL {
        let what = tier.name();
        let m = measure_crash_effect(&crash_dust(GroundClass::Mud, SHOW, tier, &earth()));
        band(&format!("{what} surge time"), m.surge_time, 14.0, 16.0);
        band(&format!("{what} surge front"), m.surge_front, 400.0, 700.0);
        band(&format!("{what} surge height"), m.surge_height, 50.0, 100.0);
    }
}

/// X2: earth and rock are thrown 200–400 m high and land 500–800 m out.
#[test]
fn x2_ejecta_rises_200_to_400_m_and_lands_500_to_800_m_out() {
    for tier in CrashTier::ALL {
        for ground in GroundClass::ALL {
            let what = format!("{} {}", tier.name(), ground.name());
            let m = measure_crash_effect(&crash_ejecta(ground, SHOW, tier, &earth()));
            band(&format!("{what} ejecta top"), m.ejecta_top, 200.0, 400.0);
            band(&format!("{what} ejecta landing"), m.ejecta_landing, 500.0, 800.0);
        }
    }
    // The hook for mesh debris: the clods fly the published launch envelope, and its ballistic
    // extremes bracket the measured reach (300-ish m up, 500–800 m out).
    let launch = SHOW.ejecta_launch();
    let def = crash_ejecta(GroundClass::Mud, SHOW, CrashTier::Full, &earth());
    let clods = def.emitters.iter().find(|e| e.name == "clods").expect("clods");
    match clods.init.velocity.mode {
        VelocityMode::Directional { spread, speed, .. } => {
            assert_eq!(spread, launch.cone_degrees);
            assert_eq!((speed.min, speed.max), (launch.min_speed, launch.max_speed));
        }
        _ => panic!("clods fly directionally"),
    }
    let g = 9.81;
    let highest = launch.max_speed * launch.max_speed / (2.0 * g);
    let furthest = launch.max_speed * launch.max_speed / g;
    assert!(highest > 300.0 && highest < 500.0, "{highest}");
    assert!(furthest > 700.0 && furthest < 900.0, "{furthest}");
    // The fountain is dark earth, also over snow (the crater digs under the cover).
    let def = crash_ejecta(GroundClass::Snow, SHOW, CrashTier::Full, &earth());
    let fountain = def
        .emitters
        .iter()
        .find(|e| e.name == "fountain")
        .expect("fountain");
    let c = crate::format::parse_color(&fountain.init.color[0].color).expect("colour");
    assert!(c[0].max(c[1]).max(c[2]) < 0.15, "dark earth: {c:?}");
}

/// X3: the cap rises at about 25 m/s, stands 800–1500 m high and 400–800 m across by 60 s,
/// and is still there after 3 minutes.
#[test]
fn x3_cap_reaches_800_to_1500_m_by_60_s_and_stays() {
    for tier in CrashTier::ALL {
        let what = tier.name();
        let m = measure_crash_effect(&crash_column(SHOW, tier, &earth()));
        band(&format!("{what} cap top 60 s"), m.column_top_60s, 800.0, 1_500.0);
        band(
            &format!("{what} cap width 60 s"),
            m.column_width_60s,
            400.0,
            800.0,
        );
        band(&format!("{what} cap rise 30 s"), m.column_rise_30s, 15.0, 35.0);
        if tier == CrashTier::Full {
            // The showcase look stays 3 minutes; Lite's cap lives 150–180 s (its smaller pool).
            band(
                &format!("{what} cap top 180 s"),
                m.column_top_180s,
                800.0,
                1_700.0,
            );
        }
    }
}

#[test]
fn x3_column_drifts_with_the_wind() {
    let calm = measure_crash_effect(&crash_column(SHOW, CrashTier::Full, &earth()));
    let mut windy = earth();
    windy.wind = Vec3::new(4.0, 0.0, 0.0);
    let blown = measure_crash_effect(&crash_column(SHOW, CrashTier::Full, &windy));
    let (c, b) = (
        calm.column_drift_60s.expect("calm drift"),
        blown.column_drift_60s.expect("blown drift"),
    );
    assert!(c[0].abs() < 30.0, "calm cap stays put: {c:?}");
    assert!(b[0] - c[0] > 60.0, "the cap drifts downwind: {b:?} vs {c:?}");
}

/// D11 (a): a short radial puff and a few clods at first contact, gone within 2 s.
#[test]
fn contact_puff_is_short_and_body_sized() {
    for tier in CrashTier::ALL {
        for ground in GroundClass::ALL {
            let what = format!("{} {}", tier.name(), ground.name());
            let m = measure_crash_effect(&crash_contact(ground, tier, &earth()));
            band(&format!("{what} contact reach"), m.contact_reach, 6.0, 40.0);
            band(&format!("{what} contact gone"), m.contact_gone_at, 0.5, 2.0);
        }
    }
}

/// X6: 30–80 fires within 300 m, burning at least 3 minutes, each lighting 30 m around it.
#[test]
fn fires_are_30_to_80_within_300_m_and_burn_3_minutes() {
    let sites = crash_fire_sites(SHOW, 11);
    band("fires", Some(sites.len() as f32), 30.0, 80.0);
    assert_eq!(sites, crash_fire_sites(SHOW, 11), "deterministic");
    assert_ne!(sites, crash_fire_sites(SHOW, 12), "the seed matters");
    // Pinned across platforms: the offsets are world positions spawned into the scene, built
    // on the deterministic sin/cos pair since the vfx1 determinism audit (finding 6).
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for site in &sites {
        for v in site.offset {
            for byte in v.to_bits().to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x100_0000_01b3);
            }
        }
    }
    assert_eq!(
        hash, FIRE_SITE_OFFSET_BITS_HASH,
        "offsets drifted from the pinned bits"
    );
    let inner = 0.5 * SHOW.crater().diameter;
    for s in &sites {
        let d = s.offset[0].hypot(s.offset[1]);
        assert!(d > inner && d <= SHOW.fire_zone() + 1e-3, "fire at {d} m");
    }
    for size in FireSize::ALL {
        assert!(sites.iter().any(|s| s.size == size), "{} fires", size.name());
        let def = crash_ground_fire(size, CrashTier::Lite, &earth());
        for e in &def.emitters {
            assert!(e.duration.is_some_and(|d| d >= 180.0), "{}/{}", def.name, e.name);
        }
        let light = def.emitters[0].render.light.clone().expect("fire light");
        assert!(light.radius >= 30.0);
        let late = at(&def, &[169.0, 169.5, 170.0, 170.5, 171.0]);
        assert!(
            late.iter()
                .any(|s| s.emitter("flames").is_some_and(|e| e.visible > 0)),
            "{} burns at 170 s",
            def.name
        );
        assert!(
            late.iter()
                .all(|s| s.emitter("smoke").is_some_and(|e| e.visible > 0)),
            "{} smokes at 170 s",
            def.name
        );
    }
    assert_eq!(CrashScale::VEHICLE.fire_count(), 3, "a small crash lights a few");
}

/// The scale parameter covers a vehicle crash to 1 kt: every layer grows with it.
#[test]
fn every_layer_grows_with_the_scale() {
    let scales = [0.05, 1.0, 10.0, 250.0, 1_000.0].map(CrashScale::from_tonnes);
    let mut last = CrashMeasure::default();
    for scale in scales {
        let fire = measure_crash_effect(&crash_fireball(scale, CrashTier::Lite, &earth()));
        let r = fire.fireball_peak_radius.expect("radius");
        assert!(
            (0.7..=1.3).contains(&(r / scale.fireball_radius())),
            "{}: peak radius {r} for R {}",
            scale.tag(),
            scale.fireball_radius()
        );
        let ejecta = measure_crash_effect(&crash_ejecta(GroundClass::Mud, scale, CrashTier::Lite, &earth()));
        if let (Some(a), Some(b)) = (last.fireball_peak_radius, fire.fireball_peak_radius) {
            assert!(b > a, "{}: fireball grows", scale.tag());
        }
        if let (Some(a), Some(b)) = (last.fireball_peak_time, fire.fireball_peak_time) {
            assert!(b > a, "{}: a larger ball peaks later", scale.tag());
        }
        if let (Some(a), Some(b)) = (last.ejecta_landing, ejecta.ejecta_landing) {
            assert!(b > a, "{}: ejecta land further", scale.tag());
        }
        last = CrashMeasure {
            ejecta_landing: ejecta.ejecta_landing,
            ..fire
        };
    }
    let vehicle = measure_crash_effect(&crash_fireball(CrashScale::VEHICLE, CrashTier::Full, &earth()));
    band("vehicle peak radius", vehicle.fireball_peak_radius, 12.0, 25.0);
    band("vehicle peak time", vehicle.fireball_peak_time, 0.3, 0.7);
}

/// The column, the fireball and the ground layers keep full detail at shot 4's 3.3 km, and the
/// column stays drawn anywhere in an 8 km scene (nothing pops in the shot).
#[test]
fn the_showcase_camera_keeps_full_detail() {
    let distance = SITE.distance(SHOT4);
    assert!((3_200.0..=3_400.0).contains(&distance), "{distance}");
    for def in crash_effects(SHOW, CrashTier::Full, &earth()) {
        assert_eq!(def.lod_scale(distance), 1.0, "{} at {distance} m", def.name);
    }
    let column = crash_column(SHOW, CrashTier::Full, &earth());
    assert!(column.lod_scale(8_192.0 * std::f32::consts::SQRT_2) > 0.0);
    let small = crash_fireball(CrashScale::VEHICLE, CrashTier::Full, &earth());
    let big = crash_fireball(SHOW, CrashTier::Full, &earth());
    let first = |d: &VfxSystemDef| d.system.lod.as_ref().map_or(0.0, |l| l.distances[0]);
    assert!(first(&big) > first(&small), "LOD distances grow with the crash");
}

fn library_scene() -> VfxScene {
    VfxScene::new(VfxLibrary::new())
}

/// D11 (a): the contact puff fires `contact_lead` (3 frames) before the detonation, the crater
/// hook comes with it, and the blast reaches shot 4's camera `distance / 343` s later (± 1
/// frame), not before.
#[test]
fn the_sequence_leads_with_contact_and_the_blast_arrives_at_the_speed_of_sound() {
    let mut scene = library_scene();
    let crash = CrashBurst {
        position: SITE,
        scale: SHOW,
        ground: GroundClass::Snow,
        ..CrashBurst::default()
    };
    let mut seq = CrashSequence::new(crash, 1.0);
    let dt = 1.0 / 60.0;
    let mut frames: Vec<(u32, CrashEventKind)> = Vec::new();
    for frame in 0..(60 * 14) {
        for kind in advance_crash_sequence(&mut scene, &mut seq, dt, CrashTier::Full, Some(SHOT4)) {
            frames.push((frame, kind));
        }
    }
    let when = |want: fn(&CrashEventKind) -> bool| frames.iter().find(|(_, k)| want(k)).map(|(f, _)| *f);
    let contact = when(|k| matches!(k, CrashEventKind::Contact)).expect("contact");
    let detonation = when(|k| matches!(k, CrashEventKind::Detonation)).expect("detonation");
    let arrival = when(|k| matches!(k, CrashEventKind::BlastArrived { .. })).expect("arrival");
    assert_eq!(detonation, 60, "detonates at impact_in");
    assert_eq!(detonation - contact, 3, "contact 3 frames (50 ms) before");
    assert!(
        frames
            .iter()
            .any(|(f, k)| *f == detonation && *k == CrashEventKind::Crater(SHOW.crater()))
    );
    let expected = blast_arrival_time(SITE, SHOT4) * 60.0;
    let got = (arrival - detonation) as f32;
    assert!(
        (got - expected).abs() <= 1.0,
        "arrival after {got} frames, expected {expected}"
    );
    assert_eq!(frames.len(), 4, "each event once: {frames:?}");
    let names: Vec<&str> = seq.instances().iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names[0], "crash_contact_snow");
    assert!(names.contains(&"crash_column@250t"));
    assert!(names.contains(&"crash_dust_snow@250t"));
    assert!(names.contains(&"crash_ejecta_snow@250t"));
    assert!(!names.contains(&"crash_shell@250t"), "no shell unless asked");
    let fires = names
        .iter()
        .filter(|n| n.starts_with("crash_ground_fire"))
        .count();
    assert_eq!(fires as u32, SHOW.fire_count());
    // The fireball sits in the air so its light reaches the land.
    let ball = seq
        .instances()
        .iter()
        .find(|(n, _)| n == "crash_fireball@250t")
        .and_then(|(_, id)| scene.instance(*id))
        .map(|i| i.origin())
        .expect("fireball");
    assert!(ball.y > SITE.y + 20.0, "{ball:?}");
}

#[test]
fn the_contact_puff_can_be_left_out_and_fires_follow_the_ground() {
    let mut scene = library_scene();
    ensure_crash_effects(&mut scene, SHOW, CrashTier::Lite, None);
    let crash = CrashBurst {
        scale: SHOW,
        contact: false,
        fireball: false,
        dust: false,
        ejecta: false,
        column: false,
        ..CrashBurst::default()
    };
    let mut seq = CrashSequence::new(crash, 0.0);
    let events = advance_crash_sequence(&mut scene, &mut seq, 1.0 / 60.0, CrashTier::Lite, None);
    assert_eq!(
        events,
        [CrashEventKind::Detonation, CrashEventKind::Crater(SHOW.crater())]
    );
    // A ground sampler that drops every other fire and lifts the rest onto a 10 m step.
    let count = std::cell::Cell::new(0u32);
    let fired = spawn_crash_on(&mut scene, &crash, &|p| {
        count.set(count.get() + 1);
        count.get().is_multiple_of(2).then_some(p + Vec3::Y * 10.0)
    });
    assert_eq!(fired.len() as u32, SHOW.fire_count() / 2);
    for (_, id) in &fired {
        let y = scene.instance(*id).map(|i| i.origin().y).unwrap_or_default();
        assert!((y - 10.0).abs() < 1e-4);
    }
}

/// The ECS driver: events and a camera shake only once the blast front reaches the camera.
#[test]
fn the_driver_shakes_the_camera_only_when_the_front_arrives() {
    let mut app = App::new();
    app.add_plugins(crate::VfxPlugin);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_secs_f64(1.0 / 60.0),
    ));
    // A camera 68.6 m away: the front takes 0.2 s (12 frames).
    let eye = Vec3::new(68.6, 0.0, 0.0);
    app.insert_resource(crate::VfxView {
        camera: crate::VfxCamera::looking_at(eye, Vec3::ZERO),
        ..crate::VfxView::default()
    });
    app.update();
    let entity = app
        .world_mut()
        .spawn(CrashSequence::new(CrashBurst::default(), 0.1))
        .id();
    let mut shaken_before = false;
    let mut arrived = false;
    for _ in 0..120 {
        app.update();
        let seq = app
            .world()
            .get::<CrashSequence>(entity)
            .cloned()
            .expect("sequence");
        let trauma = app.world().resource::<crate::shake::CameraShake>().trauma();
        if seq.arrived() {
            arrived = true;
            assert!(trauma > 0.0, "the blast shakes the camera");
            break;
        }
        shaken_before |= trauma > 0.0;
    }
    assert!(arrived, "the front reaches the camera");
    assert!(!shaken_before, "no shake before the front arrives");
}

/// A parked crash sequence (spec 37 § 13, ADR-7200): its playing effects stop, nothing fires
/// while it is parked, and its clock holds.
#[test]
fn a_parked_crash_sequence_stops_its_effects_and_holds() {
    let mut app = App::new();
    app.add_plugins(crate::VfxPlugin);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_secs_f64(1.0 / 60.0),
    ));
    app.update();
    {
        let mut scene = app.world_mut().resource_mut::<VfxScene>();
        ensure_crash_effects(&mut scene, CrashBurst::default().scale, CrashTier::Full, None);
    }
    let entity = app
        .world_mut()
        .spawn(CrashSequence::new(CrashBurst::default(), 0.05))
        .id();
    let instances = |app: &App| {
        app.world()
            .get::<CrashSequence>(entity)
            .map(|s| s.instances().to_vec())
            .unwrap_or_default()
    };
    for _ in 0..10 {
        app.update();
    }
    let playing = instances(&app);
    assert!(!playing.is_empty(), "the detonation spawned its layers");
    app.world_mut()
        .entity_mut(entity)
        .insert(regia_scene::layers::LayerParked {
            restore: regia_scene::components::Visibility::default(),
            inherited: false,
        });
    app.update();
    assert!(instances(&app).is_empty(), "a parked sequence keeps no effect");
    let scene = app.world().resource::<VfxScene>();
    for (name, id) in &playing {
        assert!(
            scene.instance(*id).is_none_or(|i| !i.is_active()),
            "{name} still emits while its member is parked"
        );
    }
    let radius = app
        .world()
        .get::<CrashSequence>(entity)
        .map(CrashSequence::front_radius);
    for _ in 0..10 {
        app.update();
    }
    assert!(instances(&app).is_empty(), "nothing fires while parked");
    assert_eq!(
        app.world()
            .get::<CrashSequence>(entity)
            .map(CrashSequence::front_radius),
        radius,
        "the clock holds while parked"
    );
}

/// Prints every measured number at the showcase scale (both tiers) and the vehicle scale, for
/// the lane report (`--nocapture`); asserts only that each layer measured something.
#[test]
fn measured_summaries_print() {
    for (scale, tier) in [
        (SHOW, CrashTier::Full),
        (SHOW, CrashTier::Lite),
        (CrashScale::VEHICLE, CrashTier::Full),
    ] {
        for def in [
            crash_fireball(scale, tier, &earth()),
            crash_shell(scale, tier),
            crash_dust(GroundClass::Snow, scale, tier, &earth()),
            crash_ejecta(GroundClass::Snow, scale, tier, &earth()),
            crash_column(scale, tier, &earth()),
            crash_contact(GroundClass::Snow, tier, &earth()),
        ] {
            let m = measure_crash_effect(&def);
            assert_ne!(m, CrashMeasure::default(), "{}", def.name);
            println!("{} {}: {m:?}", tier.name(), def.name);
        }
    }
}

/// A whole 0.25 kt crash (every layer, every fire) stays inside the particle capacity and the
/// smoke (alpha) budget of the profile its tier is for: Desktop High for full, Mobile High for
/// lite.
#[test]
fn a_whole_crash_fits_the_profile_budgets() {
    let sites = crash_fire_sites(SHOW, CrashBurst::default().seed);
    for (tier, profile) in [
        (CrashTier::Full, VfxProfile::desktop_high()),
        (CrashTier::Lite, VfxProfile::mobile_high()),
    ] {
        let times = [1.0, 5.0, 15.0, 60.0];
        let mut live = [0u32; 4];
        let mut smoke = [0u32; 4];
        let mut add = |def: &VfxSystemDef, copies: u32| {
            for (i, snap) in at(def, &times).iter().enumerate() {
                for (e, probe) in def.emitters.iter().zip(&snap.emitters) {
                    live[i] += probe.live * copies;
                    if e.render.blend == BlendMode::Alpha && e.render.mode != RenderMode::Mesh {
                        smoke[i] += probe.live * copies;
                    }
                }
            }
        };
        for def in [
            crash_fireball(SHOW, tier, &earth()),
            crash_dust(GroundClass::Snow, SHOW, tier, &earth()),
            crash_ejecta(GroundClass::Snow, SHOW, tier, &earth()),
            crash_column(SHOW, tier, &earth()),
        ] {
            add(&def, 1);
        }
        for size in FireSize::ALL {
            let n = sites.iter().filter(|s| s.size == size).count() as u32;
            add(&crash_ground_fire(size, tier, &earth()), n);
        }
        println!("{} live {live:?} smoke {smoke:?}", tier.name());
        assert!(
            live.iter().all(|&n| n <= profile.particle_capacity),
            "{} live {live:?}",
            tier.name()
        );
        assert!(
            smoke.iter().all(|&n| n <= profile.smoke_budget),
            "{} smoke {smoke:?}",
            tier.name()
        );
    }
}

/// The GPU pool is a ring: a whole 0.25 kt crash must leave its 4-minute cap alone. Its slots
/// over the longest lifetime stay under half the Desktop High pool and 90 % of Mobile High's.
#[test]
fn a_whole_crash_leaves_the_cap_in_the_particle_ring() {
    let full = crash_ring_slots(SHOW, CrashTier::Full, &earth());
    let lite = crash_ring_slots(SHOW, CrashTier::Lite, &earth());
    println!("ring slots: full {full}, lite {lite}");
    assert!(
        full * 2 <= VfxProfile::desktop_high().particle_capacity,
        "full {full}"
    );
    assert!(
        lite * 10 <= VfxProfile::mobile_high().particle_capacity * 9,
        "lite {lite}"
    );
    assert!(crash_ring_slots(CrashScale::VEHICLE, CrashTier::Lite, &earth()) < lite);
}

/// ADR-1448: the sequence's fire lights (`vfx.crash`) sit where the visible fires spawn, with
/// the same positions as `spawn_crash_on` gives the effects.
#[test]
fn the_fire_lights_follow_the_visible_fires() {
    let mut scene = library_scene();
    let crash = CrashBurst {
        position: SITE,
        scale: SHOW,
        ..CrashBurst::default()
    };
    ensure_crash_effects(&mut scene, SHOW, CrashTier::Full, None);
    let spawned = spawn_crash(&mut scene, &crash);
    let visible: Vec<Vec3> = spawned
        .iter()
        .filter(|(n, _)| n.starts_with("crash_ground_fire"))
        .filter_map(|(_, id)| scene.instance(*id).map(|i| i.origin()))
        .collect();
    let mut lights = crate::fire_light::FireLights::default();
    let added = lights.spawn_crash_sites(&crash, 0, &|p| Some(p));
    assert_eq!(added, visible.len());
    assert_eq!(added as u32, SHOW.fire_count());
    for (fire, at) in lights.fires().iter().zip(&visible) {
        assert_eq!(fire.position - Vec3::new(0.0, 0.8, 0.0), *at);
    }
}

/// ADR-1448: the driver adds the fire lights with the detonation (once), and not when the
/// sequence turns them off.
#[test]
fn the_driver_lights_the_fires_with_the_detonation() {
    for (lights_on, expected) in [(true, SHOW.fire_count() as usize), (false, 0)] {
        let mut app = App::new();
        app.add_plugins(crate::VfxPlugin);
        app.insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_secs_f64(1.0 / 60.0),
        ));
        app.update();
        let mut seq = CrashSequence::new(
            CrashBurst {
                position: SITE,
                scale: SHOW,
                ..CrashBurst::default()
            },
            0.1,
        );
        seq.fire_lights = lights_on;
        app.world_mut().spawn(seq);
        let mut counts = Vec::new();
        for _ in 0..30 {
            app.update();
            counts.push(
                app.world()
                    .resource::<crate::fire_light::FireLights>()
                    .fires()
                    .len(),
            );
        }
        assert_eq!(counts[0], 0, "no light before the detonation");
        assert_eq!(*counts.last().unwrap_or(&0), expected);
        assert!(
            counts.iter().all(|&c| c == 0 || c == expected),
            "added once: {counts:?}"
        );
    }
}

/// Chroma (max − min over max) of an sRGB hex colour such as `#464646`.
fn hex_chroma(hex: &str) -> f32 {
    let v: Vec<f32> = (0..3)
        .map(|i| f32::from(u8::from_str_radix(&hex[1 + 2 * i..3 + 2 * i], 16).unwrap_or(0)))
        .collect();
    let hi = v.iter().copied().fold(0.0, f32::max);
    let lo = v.iter().copied().fold(255.0, f32::min);
    if hi > 0.0 { (hi - lo) / hi } else { 0.0 }
}

/// ADR-1444: smoke is soot-grey, not brown: every smoke and soot emitter starts within 6 %
/// chroma of neutral.
#[test]
fn smoke_start_colours_are_neutral_grey() {
    let mut checked = 0;
    let mut check = |def: &VfxSystemDef, names: &[&str]| {
        for e in def.emitters.iter().filter(|e| names.contains(&e.name.as_str())) {
            for key in &e.init.color {
                let c = hex_chroma(&key.color);
                assert!(c < 0.06, "{} {}: chroma {c}", e.name, key.color);
                checked += 1;
            }
        }
    };
    for tier in CrashTier::ALL {
        check(&crash_fireball(SHOW, tier, &earth()), &["soot", "soot_ring"]);
        check(
            &crash_column(SHOW, tier, &earth()),
            &["cap", "stem", "crater_smoke"],
        );
    }
    assert!(checked >= 8, "{checked} colours checked");
}

/// ADR-1444: the snow dust is a cool neutral white-grey, not cream.
#[test]
fn snow_dust_is_white_grey() {
    let (dust, _, _) = GroundClass::Snow.colors();
    assert!(hex_chroma(dust) < 0.06, "{dust}");
}

/// ADR-1444: the ground class follows the receiving ground's albedo.
#[test]
fn ground_class_follows_the_albedo() {
    assert_eq!(GroundClass::from_albedo([0.7, 0.72, 0.75]), GroundClass::Snow);
    assert_eq!(GroundClass::from_albedo([0.08, 0.06, 0.04]), GroundClass::Mud);
    assert_eq!(GroundClass::from_albedo([0.3, 0.32, 0.12]), GroundClass::Grass);
    assert_eq!(GroundClass::from_albedo([0.0, 0.0, 0.0]), GroundClass::Mud);
}

/// ADR-1444: the flash falls off with distance (inverse square inside a radius reaching past
/// the farthest viewer) and does not light a 3.3 km valley at more than a tenth of the sun:
/// a gradient, not a wash.
#[test]
fn flash_is_a_gradient_not_a_wash_at_the_shot_distance() {
    let def = crash_fireball(SHOW, CrashTier::Full, &earth());
    let light = def
        .emitters
        .iter()
        .find(|e| e.name == "flash")
        .and_then(|e| e.render.light.clone())
        .expect("flash light");
    assert!(light.radius >= 6_000.0, "radius {}", light.radius);
    let lumens = light.intensity * crate::runtime::PARTICLE_LIGHT_LUMENS_PER_UNIT;
    let lux = |d: f32| lumens / (4.0 * std::f32::consts::PI * d * d);
    let at_shot = lux(SITE.distance(SHOT4));
    assert!(at_shot < 10_000.0, "{at_shot} lx at the shot-4 camera distance");
    // Across the valley (2.8 to 3.8 km from the burst) the light drops by at least 40 %.
    assert!(lux(3_800.0) < 0.6 * lux(2_800.0));
}

/// Linear albedo (luma of the init colour × the colour over life) of emitter `e` at life
/// fraction `t`.
fn albedo_at(e: &EmitterDef, t: f32) -> f32 {
    let init = crate::format::parse_color(&e.init.color[0].color).expect("init colour");
    let over = crate::curves::sample_color(&e.update.color_over_life, t);
    crate::quality::luma([init[0] * over[0], init[1] * over[1], init[2] * over[2]])
}

fn emitter<'a>(def: &'a VfxSystemDef, name: &str) -> &'a EmitterDef {
    def.emitters
        .iter()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("{}: no {name}", def.name))
}

/// ADR-1472: an energy-store vent over snow lifts condensation, snow powder and dust, so the cap
/// is grey-white (albedo 0.4 and up as it leaves the fireball, 0.55 and up by the end of the
/// showcase shot, never above fresh snow) and neutral, and it only lightens as it dilutes.
#[test]
fn the_cap_is_condensation_and_dust_not_soot() {
    for tier in CrashTier::ALL {
        let column = crash_column(SHOW, tier, &earth());
        for name in ["cap", "cap_veil", "cap_wisps"] {
            let e = emitter(&column, name);
            let what = format!("{} {name}", tier.name());
            let life = 0.5 * (e.spawn.lifetime.min + e.spawn.lifetime.max);
            // The showcase shot ends about 21 s after the detonation.
            let shot_end = 21.0 / life;
            assert!(albedo_at(e, 0.0) >= 0.4, "{what}: {}", albedo_at(e, 0.0));
            assert!(
                albedo_at(e, shot_end) >= 0.55,
                "{what}: {}",
                albedo_at(e, shot_end)
            );
            let mut last = 0.0;
            for i in 0..=40 {
                let t = i as f32 / 40.0;
                let a = albedo_at(e, t);
                assert!(a + 1.0e-6 >= last, "{what}: darkens at t={t}");
                assert!(a <= 0.8, "{what}: brighter than snow at t={t}: {a}");
                last = a;
                let init = crate::format::parse_color(&e.init.color[0].color).expect("init");
                let over = crate::curves::sample_color(&e.update.color_over_life, t);
                let c = [init[0] * over[0], init[1] * over[1], init[2] * over[2]];
                let spread = c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2]);
                assert!(spread < 0.02, "{what}: not neutral at t={t}: {c:?}");
            }
            assert!(e.render.lit, "{what}");
        }
    }
}

/// ADR-1472: the stem is dirty snow and dust: darker at the crater than the cap (0.1–0.25),
/// lighter with height (0.45 and up under the cap), and lighter than the crater's smoke.
#[test]
fn the_stem_is_dark_at_the_crater_and_lightens_with_height() {
    for tier in CrashTier::ALL {
        let column = crash_column(SHOW, tier, &earth());
        let stem = emitter(&column, "stem");
        let base = albedo_at(stem, 0.0);
        assert!((0.1..=0.25).contains(&base), "{} stem base {base}", tier.name());
        assert!(albedo_at(stem, 1.0) >= 0.45, "{} stem top", tier.name());
        assert!(base < albedo_at(emitter(&column, "cap"), 0.0));
        assert!(albedo_at(emitter(&column, "crater_smoke"), 0.0) < base);
        let mut last = 0.0;
        for i in 0..=20 {
            let a = albedo_at(stem, i as f32 / 20.0);
            assert!(a + 1.0e-6 >= last);
            last = a;
        }
    }
}

/// ADR-1472: less soot than a fuel fire. The fireball's soot stays carbon-black through `τ`
/// (X1: sooty by 3–5 s) and then lifts toward grey as it mixes into the condensation; there are
/// fewer soot puffs than the 56 / 24 of the soot-cap look.
#[test]
fn soot_is_black_at_first_and_greys_as_it_mixes() {
    for (tier, most) in [(CrashTier::Full, 40), (CrashTier::Lite, 18)] {
        let fireball = crash_fireball(SHOW, tier, &earth());
        let soot = emitter(&fireball, "soot");
        // ADR-1498 lays the soot down in two populations (the shell and the rolling ring): the
        // count bound holds for both together.
        let n: u32 = ["soot", "soot_ring"]
            .iter()
            .map(|name| match emitter(&fireball, name).spawn.mode {
                SpawnMode::Burst(n) => n,
                _ => panic!("{name} is a burst"),
            })
            .sum();
        assert!(n <= most, "{} soot {n}", tier.name());
        let tau = SHOW.time_scale();
        let shortest = soot.spawn.lifetime.min;
        // Age τ − delay, on the shortest life (the latest point in its ramp at τ).
        let at_tau = (tau - soot.delay) / shortest;
        assert!(
            albedo_at(soot, at_tau) < 0.05,
            "{}: {}",
            tier.name(),
            albedo_at(soot, at_tau)
        );
        assert!(albedo_at(soot, 1.0) >= 0.2, "{}", tier.name());
        assert!(albedo_at(soot, 1.0) < albedo_at(emitter(&crash_column(SHOW, tier, &earth()), "cap"), 0.0));
    }
}

/// ADR-1472: the cap is lumpy at several scales: its three populations start from 0.2 R to
/// 1.2 R (a 4× spread or more), the body alone spans 1.7× or more, the wisps grow faster than
/// the body, and the veil is thinner than the body. The puff count is the soot-cap look's 110 /
/// 44, so the frame cost stays put.
#[test]
fn the_cap_mixes_puff_sizes_and_keeps_its_count() {
    let big_r = SHOW.fireball_radius();
    for (tier, total) in [(CrashTier::Full, 110), (CrashTier::Lite, 44)] {
        let column = crash_column(SHOW, tier, &earth());
        let names = ["cap", "cap_veil", "cap_wisps"];
        let mut smallest = f32::MAX;
        let mut largest = 0.0f32;
        let mut count = 0;
        for name in names {
            let e = emitter(&column, name);
            smallest = smallest.min(e.init.size.min);
            largest = largest.max(e.init.size.max);
            if let SpawnMode::Burst(n) = e.spawn.mode {
                count += n;
            }
        }
        let what = tier.name();
        assert_eq!(count, total, "{what}: cap puffs");
        assert!(largest >= 4.0 * smallest, "{what}: {smallest}..{largest}");
        assert!(smallest <= 0.25 * big_r && largest >= big_r, "{what}");
        let body = emitter(&column, "cap");
        assert!(body.init.size.max >= 1.7 * body.init.size.min, "{what}: body");
        let grow = |e: &EmitterDef| crate::curves::sample_scalar(&e.update.size_over_life, 1.0 / 7.0, 1.0);
        assert!(grow(emitter(&column, "cap_wisps")) > grow(body), "{what}: wisps");
        let peak = |e: &EmitterDef| {
            e.update
                .alpha_over_life
                .iter()
                .map(|k| k.v)
                .fold(0.0f32, f32::max)
        };
        assert!(
            peak(emitter(&column, "cap_veil")) < 0.5 * peak(body),
            "{what}: veil"
        );
    }
}

/// ADR-1472: under the look rounds' light (a warm 21° sun, a blue sky, a warm ground) the cap's
/// sky-lit core at the end of the shot is several times brighter than the soot cap's, and its
/// sun-lit rind is grey-white, not brown.
#[test]
fn the_lit_cap_is_grey_white_not_brown() {
    use crate::quality::{VolumetricPuff, luma, puff_shade_volumetric};
    let column = crash_column(SHOW, CrashTier::Full, &earth());
    let cap = emitter(&column, "cap");
    let life = 0.5 * (cap.spawn.lifetime.min + cap.spawn.lifetime.max);
    let albedo = albedo_at(cap, 21.0 / life);
    // The soot cap's albedo at the same age: init #686868 × a #9a9a9a → #e0e0e0 ramp.
    let soot_cap = {
        let init = crate::format::parse_color("#686868").expect("hex")[0];
        let lo = crate::format::parse_color("#9a9a9a").expect("hex")[0];
        let hi = crate::format::parse_color("#e0e0e0").expect("hex")[0];
        init * (lo + (hi - lo) * 21.0 / life)
    };
    let shade = |uv: [f32; 2], opacity: f32, a: f32| {
        let s = puff_shade_volumetric(&VolumetricPuff {
            uv,
            opacity,
            albedo: [a; 3],
            self_shadow: cap.render.self_shadow,
            sun_dir_view: [-0.93, 0.36, -0.1],
            sun_rgb: [0.637, 0.473, 0.3],
            up_view: [0.0, 1.0, 0.0],
            sky_up: [0.0763, 0.0973, 0.1353],
            sky_down: [0.0924, 0.0808, 0.0733],
        });
        s.map(|c| c * a)
    };
    let core = shade([0.0, 0.0], 0.8, albedo);
    let old_core = shade([0.0, 0.0], 0.8, soot_cap);
    assert!(
        luma(core) > 5.0 * luma(old_core),
        "core {core:?} soot cap {old_core:?}"
    );
    let rind = shade([-0.85, 0.1], 0.5, albedo);
    assert!(rind[0] / rind[2] < 1.35, "rind {rind:?}");
    assert!(luma(rind) > luma(core), "the sun side stays the bright side");
}

/// Look round 5: the aging cap spreads wider and softer, the young cap is untouched. Up to
/// `CAP_YOUNG_UNTIL` of its life every puff's size and opacity keys equal the authored ones; after
/// it the size grows and the opacity falls by the same factor, which is 1 + `CAP_AGE_DIFFUSION`
/// at the end of life. A diffusion of 0 reproduces the old keys.
#[test]
fn aging_cap_puffs_diffuse_and_young_puffs_do_not() {
    assert_eq!(cap_age_growth(0.0, CAP_AGE_DIFFUSION), 1.0);
    assert_eq!(cap_age_growth(CAP_YOUNG_UNTIL, CAP_AGE_DIFFUSION), 1.0);
    assert!((cap_age_growth(1.0, CAP_AGE_DIFFUSION) - (1.0 + CAP_AGE_DIFFUSION)).abs() < 1e-6);
    let mut last = 1.0;
    for i in 0..=100 {
        let g = cap_age_growth(i as f32 / 100.0, CAP_AGE_DIFFUSION);
        assert!(g >= last, "never un-grows");
        last = g;
    }
    let size = [(0.0, 0.5), (CAP_YOUNG_UNTIL, 1.6), (3.0 / 7.0, 2.6), (1.0, 3.4)];
    assert_eq!(
        cap_aged_keys(&size, 0.0, true),
        size.to_vec(),
        "0 is the old look"
    );
    let aged = cap_aged_keys(&size, CAP_AGE_DIFFUSION, true);
    assert_eq!(aged[..2], size[..2], "young keys are bit-identical");
    assert!(aged[2].1 > size[2].1 && aged[3].1 > size[3].1);
    let alpha = cap_aged_keys(
        &[(CAP_YOUNG_UNTIL, 0.8), (6.0 / 7.0, 0.3), (1.0, 0.0)],
        CAP_AGE_DIFFUSION,
        false,
    );
    assert_eq!(alpha[0].1, 0.8);
    assert!(alpha[1].1 < 0.3 && alpha[2].1 == 0.0);
    // The emitters use it: a cap puff's size at the end of life is the authored one times the
    // growth, and its key at the end of the young phase is the authored one.
    for tier in [CrashTier::Full, CrashTier::Lite] {
        let column = crash_column(SHOW, tier, &earth());
        for (name, authored_end, authored_young) in
            [("cap", 3.4, 1.6), ("cap_veil", 2.9, 1.5), ("cap_wisps", 4.8, 2.4)]
        {
            let keys = &emitter(&column, name).update.size_over_life;
            let end = keys.last().expect("keys").v;
            assert!(
                (end - authored_end * (1.0 + CAP_AGE_DIFFUSION)).abs() < 1e-4,
                "{name}: {end}"
            );
            let young = keys.iter().find(|k| k.t == CAP_YOUNG_UNTIL).expect("key").v;
            assert_eq!(young, authored_young, "{name}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// ADR-1497: the blast front's one clock, the dust wall and the gust line.

/// X2 (ADR-1497): a dust wall rises along the front's path out to 300–500 m at 250 t, grows
/// outward at the speed of sound (its edge never more than a ring ratio and a frame behind the
/// front) and never leads it, on both tiers and every ground.
#[test]
fn x2_the_dust_wall_runs_with_the_front_to_300_to_500_m_and_never_leads() {
    for tier in CrashTier::ALL {
        for ground in GroundClass::ALL {
            let what = format!("{} {}", tier.name(), ground.name());
            let def = crash_dust(ground, SHOW, tier, &earth());
            let rings = SHOW.wall_rings(tier);
            let walls = def
                .emitters
                .iter()
                .filter(|e| e.name.starts_with(WALL_EMITTER_PREFIX))
                .count();
            assert_eq!(walls, rings.len(), "{what}: one emitter per ring");
            assert!(rings[0] < 150.0, "{what}: the wall is up by 150 m");
            let m = measure_crash_effect(&def);
            band(&format!("{what} wall reach"), m.wall_reach, 300.0, 500.0);
            assert_eq!(
                m.wall_leads_front,
                Some(false),
                "{what}: the wall leads the front"
            );
            let lag = band(&format!("{what} wall lag"), m.wall_lag, 0.0, 1.0);
            let ratio = if tier == CrashTier::Full {
                WALL_RING_RATIO
            } else {
                1.15
            };
            // One ring ratio plus two frames of the front at 150 m.
            let bound = 1.0 - 1.0 / ratio + 2.0 * 343.0 / 60.0 / 150.0;
            assert!(lag <= bound, "{what}: wall lag {lag} over {bound}");
            band(&format!("{what} wall height"), m.wall_height, 10.0, 90.0);
            println!("{what}: {m:?}");
        }
    }
    // The X2 check itself stays on the front: ±10 % beyond 150 m on Full (the ring ratio is
    // 1.075, the edge lags by at most that and a frame).
    let full = measure_crash_effect(&crash_dust(GroundClass::Snow, SHOW, CrashTier::Full, &earth()));
    band("full wall lag", full.wall_lag, 0.0, 0.1);
}

/// The wall never leads the front at other frame rates either: the implicit drag lands a puff
/// at its ring from below at any step.
#[test]
fn the_dust_wall_stays_behind_the_front_at_30_and_120_hz() {
    let def = crash_dust(GroundClass::Snow, SHOW, CrashTier::Full, &earth());
    for hz in [30.0f32, 120.0] {
        let settings = ProbeSettings {
            dt: 1.0 / hz,
            ..ProbeSettings::default()
        };
        let times: Vec<f32> = (1..=(2.0 * hz) as u32).map(|i| i as f32 / hz).collect();
        for snap in probe_effect(&def, Vec3::ZERO, Vec3::Y, settings, &times) {
            let front = blast_front_radius(snap.time);
            for e in snap
                .emitters
                .iter()
                .filter(|e| e.name.starts_with(WALL_EMITTER_PREFIX))
            {
                assert!(
                    e.visible == 0 || e.front_p90 <= front + 2.0,
                    "{hz} Hz at {} s: {} at {} ahead of the front at {front}",
                    snap.time,
                    e.name,
                    e.front_p90
                );
            }
        }
    }
}

/// One clock: every wall ring leaves on the front's schedule (its delay plus its lead is its
/// radius over the front speed), and its puffs rest at most at the ring.
#[test]
fn the_wall_rings_spawn_on_the_fronts_clock() {
    for tier in CrashTier::ALL {
        let def = crash_dust(GroundClass::Mud, SHOW, tier, &earth());
        for (e, r) in def
            .emitters
            .iter()
            .filter(|e| e.name.starts_with(WALL_EMITTER_PREFIX))
            .zip(SHOW.wall_rings(tier))
        {
            let VelocityMode::Planar { speed, .. } = &e.init.velocity.mode else {
                panic!("{}: planar", e.name);
            };
            assert!((speed.max / e.update.drag - r).abs() < 1e-2, "{}", e.name);
            let lead = blast_arrival_seconds(r) as f32 - e.delay;
            assert!((0.0..=0.03).contains(&lead), "{}: lead {lead}", e.name);
        }
    }
}

/// What [`one_clock_run`] saw: the scene, the sequence, the events by frame, the frames the
/// gust blew at the listener, and the frame each gust puff fired on.
struct OneClock {
    scene: VfxScene,
    seq: CrashSequence,
    events: Vec<(u32, CrashEventKind)>,
    gust_frames: Vec<u32>,
    puff_frames: Vec<u32>,
}

fn one_clock_run(listener: Vec3, sites: Vec<Vec3>) -> OneClock {
    let mut scene = library_scene();
    let crash = CrashBurst {
        position: SITE,
        scale: SHOW,
        ground: GroundClass::Snow,
        fires: false,
        column: false,
        ..CrashBurst::default()
    };
    let mut seq = CrashSequence::new(crash, 0.5);
    seq.gust_sites = sites;
    let dt = 1.0 / 60.0;
    let mut events = Vec::new();
    let mut gust_frames = Vec::new();
    let mut puff_frames = Vec::new();
    for frame in 0..(60 * 13) {
        let puffs = seq.gust_puffs();
        for kind in advance_crash_sequence(&mut scene, &mut seq, dt, CrashTier::Full, Some(listener)) {
            events.push((frame, kind));
        }
        puff_frames.extend(std::iter::repeat_n(frame, (seq.gust_puffs() - puffs) as usize));
        let wind = seq
            .active_front(Entity::PLACEHOLDER, dt)
            .map_or(Vec3::ZERO, |f| f.wind_at(listener));
        if wind.length() > 0.0 {
            gust_frames.push(frame);
        }
    }
    OneClock {
        scene,
        seq,
        events,
        gust_frames,
        puff_frames,
    }
}

/// ADR-1497, the owner's check: the camera shake (`BlastArrived`), the gust at the camera and
/// the front radius all come from one clock. At shot 4's 3.3 km camera they land on one frame,
/// the frame `distance / 343` after the detonation rounds up to (the framework's shock-shake rule).
#[test]
fn the_shake_the_gust_and_the_front_reach_the_camera_on_one_frame() {
    let OneClock {
        events, gust_frames, ..
    } = one_clock_run(SHOT4, Vec::new());
    let when = |want: fn(&CrashEventKind) -> bool| events.iter().find(|(_, k)| want(k)).map(|(f, _)| *f);
    let detonation = when(|k| matches!(k, CrashEventKind::Detonation)).expect("detonation");
    let arrival = when(|k| matches!(k, CrashEventKind::BlastArrived { .. })).expect("arrival");
    let distance = SITE.distance(SHOT4);
    assert!((3_300.0..3_320.0).contains(&distance), "{distance}");
    assert_eq!(
        u64::from(arrival - detonation),
        blast_arrival_step(distance, 60.0),
        "the shake frame is the front's"
    );
    assert_eq!(arrival - detonation, 579, "9.65 s at 60 Hz");
    assert_eq!(
        gust_frames.first().copied(),
        Some(arrival),
        "the gust reaches the camera with the shake"
    );
    // The front is past the camera on that frame and short of it the frame before.
    let at = |frame: u32| blast_front_radius((frame - detonation) as f32 / 60.0);
    assert!(at(arrival) + 0.01 >= distance && at(arrival - 1) < distance);
}

/// The gust line: a puff at each gust site on the frame the front reaches it, nearest the camera
/// first when there are more than the cap, deterministic.
#[test]
fn gust_puffs_fire_as_the_front_passes_each_site() {
    let dir = (SHOT4 - SITE).normalize();
    let sites: Vec<Vec3> = [600.0f32, 1_200.0, 2_400.0, 3_200.0]
        .iter()
        .map(|d| SITE + dir * *d)
        .collect();
    let OneClock {
        scene,
        seq,
        events,
        puff_frames,
        ..
    } = one_clock_run(SHOT4, sites.clone());
    let detonation = events
        .iter()
        .find(|(_, k)| *k == CrashEventKind::Detonation)
        .map(|(f, _)| *f)
        .expect("detonation");
    let puffs: Vec<(String, u64)> = seq
        .instances()
        .iter()
        .filter(|(n, _)| n == CRASH_GUST_SNOW)
        .cloned()
        .collect();
    assert_eq!(puffs.len(), 4);
    assert_eq!(seq.gust_puffs(), 4);
    for ((_, id), site) in puffs.iter().zip(&sites) {
        let inst = scene.instance(*id).expect("live puff");
        assert!(inst.origin().distance(*site) < 1e-3);
        // Launched along the blast wind, tilted up.
        let out = (*site - SITE).with_y(0.0).normalize();
        let (_, _, forward) = inst.axes();
        assert!(forward.dot(out) > 0.7 && forward.y > 0.5, "{forward:?}");
    }
    // A site fires on the frame the front reaches it (the shake's rounding rule).
    let expect: Vec<u32> = sites
        .iter()
        .map(|site| detonation + blast_arrival_step(SITE.distance(*site), 60.0) as u32)
        .collect();
    assert_eq!(puff_frames, expect);
    // More sites than the cap: the nearest to the camera fire.
    let many: Vec<Vec3> = (0..(CrashSequence::MAX_GUST_PUFFS + 20))
        .map(|i| SITE + dir * (100.0 + 60.0 * i as f32))
        .collect();
    let seq = one_clock_run(SHOT4, many).seq;
    assert_eq!(seq.gust_puffs() as usize, CrashSequence::MAX_GUST_PUFFS);
    // Off: nothing fires.
    let mut scene = library_scene();
    let mut off = CrashSequence::new(
        CrashBurst {
            gust: false,
            ..CrashBurst::default()
        },
        0.0,
    );
    off.gust_sites = vec![Vec3::new(1.0, 0.0, 0.0)];
    for _ in 0..10 {
        advance_crash_sequence(&mut scene, &mut off, 1.0 / 60.0, CrashTier::Lite, None);
    }
    assert_eq!(off.gust_puffs(), 0);
}

/// The contact puff tilts downrange by half the body's angle from the vertical (a host's
/// approach fills `contact_downrange`); straight up without one or for a vertical fall.
#[test]
fn the_contact_puff_tilts_downrange_of_a_slanting_fall() {
    let el = 70f32.to_radians();
    // From the north-east (azimuth 40), travelling south-west and down.
    let from = Vec3::new(40f32.to_radians().sin(), 0.0, 40f32.to_radians().cos());
    let travel = -from * el.cos() - Vec3::Y * el.sin();
    let crash = CrashBurst {
        position: SITE,
        contact_downrange: Some(travel),
        ..CrashBurst::default()
    };
    let forward = contact_forward(&crash);
    assert!(
        (forward.angle_between(Vec3::Y).to_degrees() - 10.0).abs() < 0.01,
        "{forward:?}"
    );
    assert!(forward.dot(-from) > 0.0, "tilted downrange");
    assert_eq!(contact_forward(&CrashBurst::default()), Vec3::Y);
    for t in [-Vec3::Y, Vec3::Y, Vec3::new(f32::NAN, 0.0, 0.0), Vec3::ZERO] {
        let c = CrashBurst {
            contact_downrange: Some(t),
            ..CrashBurst::default()
        };
        assert!(contact_forward(&c).angle_between(Vec3::Y) < 1e-4, "{t:?}");
    }
}

/// The grass bends to the strongest front near the camera, by about 0.4 of the blade height at
/// shot 4's 3.3 km, and lets go once the tail is past.
#[test]
fn the_grass_gust_follows_the_front_at_the_camera() {
    assert_eq!(grass_gust_bend(0.0), 0.0);
    assert_eq!(grass_gust_bend(f32::NAN), 0.0);
    assert!((grass_gust_bend(60.0) - 0.7).abs() < 1e-6, "saturates");
    let d = SITE.distance(SHOT4);
    let front = |elapsed: f64| ActiveFront {
        source: Entity::PLACEHOLDER,
        origin: SITE,
        normal: Vec3::Y,
        scale: SHOW,
        elapsed,
        prev_elapsed: elapsed - 1.0 / 60.0,
    };
    let arrive = blast_arrival_seconds(d);
    let gust = grass_gust_for(&[front(arrive)], SHOT4).expect("gust");
    assert_eq!(gust.center, [SITE.x, SITE.z]);
    assert!((gust.radius - d).abs() < 0.5);
    assert!(gust.prev_radius < gust.radius);
    assert!((0.3..=0.5).contains(&gust.bend), "{gust:?}");
    assert!((40.0..=250.0).contains(&gust.length), "{gust:?}");
    assert_eq!(grass_gust_for(&[front(arrive + 10.0)], SHOT4), None, "tail past");
    assert_eq!(grass_gust_for(&[], SHOT4), None);
}

/// The ECS path: `publish_blast_gusts` lists the front on the frame the driver runs it, and the
/// wind reaches the camera on the update that shakes it, not before.
#[test]
fn the_published_gust_reaches_the_camera_with_the_shake() {
    let mut app = App::new();
    app.add_plugins(crate::VfxPlugin);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_secs_f64(1.0 / 60.0),
    ));
    let eye = Vec3::new(68.6, 0.0, 0.0);
    app.insert_resource(crate::VfxView {
        camera: crate::VfxCamera::looking_at(eye, Vec3::ZERO),
        ..crate::VfxView::default()
    });
    app.update();
    let entity = app
        .world_mut()
        .spawn(CrashSequence::new(CrashBurst::default(), 0.1))
        .id();
    let mut arrived = false;
    for _ in 0..120 {
        app.update();
        let seq = app
            .world()
            .get::<CrashSequence>(entity)
            .cloned()
            .expect("sequence");
        let wind = app.world().resource::<BlastGusts>().wind_at(eye);
        if seq.detonated() {
            assert_eq!(app.world().resource::<BlastGusts>().fronts().len(), 1);
        }
        if seq.arrived() {
            arrived = true;
            assert!(wind.length() > 0.0, "the gust arrives with the shake");
            break;
        }
        assert_eq!(wind, Vec3::ZERO, "no gust before the front");
    }
    assert!(arrived);
}

/// The gust puff is white powder: no light and no glow (D11: nothing glows below 20 km).
#[test]
fn the_gust_puff_does_not_glow() {
    for tier in CrashTier::ALL {
        let def = crash_gust_snow(tier, &earth());
        def.validate().unwrap_or_else(|e| panic!("{}: {e}", def.name));
        for e in &def.emitters {
            assert!(e.render.light.is_none(), "{}", def.name);
            assert_ne!(e.render.blend, BlendMode::Additive, "{}", def.name);
            assert!(e.render.emissive <= 1.0, "{}", def.name);
        }
    }
}
