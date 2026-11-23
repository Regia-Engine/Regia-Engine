// SPDX-License-Identifier: MPL-2.0
//! Test-gap audit (lane testgaps-fluid-2d-xr): the `.fluidcache` writer and reader beyond the
//! acceptance round trip: varying particle counts, random access, key frames, limits and hostile bytes.

use glam::Vec3;
use regia_fluid::cache::{
    CacheError, FLAG_WHITEWATER, FLUIDCACHE_FORMAT_VERSION, FLUIDCACHE_MAGIC, MAX_FRAMES, cook,
    fluidcache_file_version, fluidcache_migration_sources, fluidcache_schema_fingerprint,
};
use regia_fluid::sim::{Emitter, EmitterKind, FluidParams, FluidSim, Shape};
use regia_fluid::{FluidCache, PointKind};

fn sim(whitewater: bool) -> FluidSim {
    let mut params = FluidParams {
        bounds_min: Vec3::ZERO,
        bounds_max: Vec3::new(0.5, 0.5, 0.5),
        cell_size: 0.0625,
        ..FluidParams::default()
    };
    params.whitewater.enabled = whitewater;
    let mut s = FluidSim::new(params).expect("sim");
    s.add_emitter(Emitter {
        shape: Shape::Box {
            center: Vec3::new(0.2, 0.3, 0.25),
            half_extents: Vec3::new(0.15, 0.2, 0.2),
        },
        velocity: Vec3::new(1.5, 0.0, 0.0),
        kind: EmitterKind::Fill,
        done: false,
    })
    .expect("emitter");
    s
}

fn close(a: [f32; 3], b: [f32; 3], q: f32) -> bool {
    (0..3).all(|i| (a[i] - b[i]).abs() <= q * 0.5001)
}

/// Particle counts that grow and shrink between frames (a delta frame has no previous point for
/// the new ones; a shrink leaves previous points unused) decode exactly, in any access order,
/// across several key frames.
#[test]
fn varying_counts_round_trip_in_any_access_order() {
    let mut s = sim(false);
    let mut cache = FluidCache::for_sim(&s, false);
    cache.keyframe_interval = 3;
    let mut expected = Vec::new();
    for step in 0..10 {
        s.step();
        match step {
            2 => {
                s.add_particles(&[(Vec3::splat(0.4), Vec3::ZERO); 7]);
            }
            5 => {
                let n = s.positions().len() / 2;
                let (p, v) = (s.positions()[..n].to_vec(), s.velocities()[..n].to_vec());
                s.set_particles(p, v);
            }
            7 => {
                s.set_particles(Vec::new(), Vec::new());
            }
            _ => {}
        }
        cache.record(&s).expect("record");
        expected.push(cache.quantized_frame(&s));
    }
    assert_eq!(cache.frame_count(), 10);
    assert!(expected[8].is_empty(), "the emptied frame is empty");
    let bytes = cache.to_bytes();
    let back = FluidCache::from_bytes(&bytes).expect("read back");
    assert_eq!(back, cache, "the reader rebuilds the writer's state");
    assert_eq!(back.to_bytes(), bytes, "canonical form is stable");
    for i in (0..10).rev().chain(0..10) {
        let f = back.play(i).expect("play");
        let e = &expected[i];
        assert_eq!(f.step, e.step, "frame {i}");
        assert_eq!(f.kinds, e.kinds, "frame {i}");
        assert_eq!(f.positions.len(), e.positions.len(), "frame {i}");
        for (a, b) in f.positions.iter().zip(&e.positions) {
            assert_eq!(a, b, "frame {i}: replay equals the quantized live state exactly");
        }
    }
}

/// Quantization error is at most half a quantum for points inside the domain, and points outside
/// clamp to the quantization box instead of wrapping.
#[test]
fn quantization_error_and_clamping() {
    let mut s = sim(false);
    let mut cache = FluidCache::for_sim(&s, false);
    s.set_particles(
        vec![
            Vec3::new(0.1234567, 0.3333333, 0.4999999),
            Vec3::new(0.25, 0.25, 0.25),
        ],
        vec![Vec3::ZERO; 2],
    );
    cache.record(&s).expect("record");
    let f = cache.play(0).expect("frame");
    for (got, want) in f.positions.iter().zip(s.positions()) {
        assert!(
            close(*got, want.to_array(), cache.quantum),
            "{got:?} vs {want:?} (quantum {})",
            cache.quantum
        );
    }
    assert!(
        (cache.quantum - 0.5 / 65_535.0).abs() < 1e-9,
        "the quantum spans the domain's longest edge"
    );
    // Positions the simulator would never hold: far outside, NaN. The writer clamps and zeroes.
    let mut wild = sim(false);
    wild.set_particles(
        vec![Vec3::new(-9.0, 9.0, 0.0), Vec3::new(f32::NAN, 0.1, 0.1)],
        vec![Vec3::ZERO; 2],
    );
    let mut c = FluidCache::for_sim(&wild, false);
    c.record(&wild).expect("wild frames record");
    let f = c.play(0).expect("frame");
    assert_eq!(
        f.positions[0],
        [0.0, 0.5, 0.0],
        "x clamps to the minimum, y to the maximum corner of the box"
    );
    assert_eq!(f.positions[1][0], 0.0, "NaN quantizes to the origin");
}

/// Key frames fall on multiples of the interval, delta frames are smaller than the key frame for a
/// slowly moving body, and an interval of 0 is not producing a file the reader refuses.
#[test]
fn key_frames_delta_size_and_interval_zero() {
    let mut s = sim(false);
    let mut c = FluidCache::for_sim(&s, false);
    c.keyframe_interval = 4;
    for _ in 0..9 {
        s.step();
        c.record(&s).expect("record");
    }
    let bytes = c.to_bytes();
    // Frame payloads start after the 52-byte header; each is `len:u32` + payload, first byte 0 = key.
    let mut at = 52;
    let mut kinds = Vec::new();
    let mut sizes = Vec::new();
    while at < bytes.len() {
        let len = u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize;
        kinds.push(bytes[at + 4]);
        sizes.push(len);
        at += 4 + len;
    }
    assert_eq!(at, bytes.len());
    assert_eq!(kinds, vec![0, 1, 1, 1, 0, 1, 1, 1, 0], "keys at 0, 4, 8");
    assert!(
        sizes[1] < sizes[0],
        "a delta frame ({}) is smaller than its key frame ({})",
        sizes[1],
        sizes[0]
    );

    let mut z = FluidCache::for_sim(&s, false);
    z.keyframe_interval = 0;
    z.record(&s).expect("record treats 0 as 1");
    z.record(&s).expect("record");
    let zero = z.to_bytes();
    assert!(
        FluidCache::from_bytes(&zero).is_ok(),
        "a cache recorded with keyframe_interval 0 must still read back: {:?}",
        FluidCache::from_bytes(&zero).err()
    );
}

/// Recording with the public field at 0 normalizes it in memory, so the writer state, the file
/// bytes and a re-read cache compare equal (the reader rejects 0, and `FluidCache` derives
/// `PartialEq` over the field).
#[test]
fn interval_zero_normalizes_and_round_trips_equal() {
    let mut s = sim(false);
    let mut c = FluidCache::for_sim(&s, false);
    c.keyframe_interval = 0;
    s.step();
    c.record(&s).expect("record");
    assert_eq!(c.keyframe_interval, 1, "record normalizes 0 to 1");
    let bytes = c.to_bytes();
    assert_eq!(
        u32::from_le_bytes([bytes[40], bytes[41], bytes[42], bytes[43]]),
        1,
        "the file says 1"
    );
    let back = FluidCache::from_bytes(&bytes).expect("read back");
    assert_eq!(back, c, "writer, file and reader all agree");
}

/// Whitewater points survive the file: kinds and ages (to a hundredth of a second).
#[test]
fn whitewater_round_trip_keeps_kind_and_age() {
    let mut s = sim(true);
    s.params().whitewater.validate().expect("defaults validate");
    let mut c = FluidCache::for_sim(&s, true);
    assert!(c.has_whitewater());
    assert_eq!(c.flags, FLAG_WHITEWATER);
    let mut saw = 0;
    for _ in 0..90 {
        s.step();
        saw += s.whitewater().particles().len();
        c.record(&s).expect("record");
    }
    assert!(saw > 0, "the scene made whitewater, so this test is not vacuous");
    let back = FluidCache::from_bytes(&c.to_bytes()).expect("read");
    for i in [10usize, 45, 89] {
        let live = c.play(i).expect("play");
        let read = back.play(i).expect("play back");
        assert_eq!(live, read);
        let liquid = live.count(PointKind::Liquid);
        assert_eq!(
            live.len() - liquid,
            live.count(PointKind::Foam) + live.count(PointKind::Spray) + live.count(PointKind::Bubble)
        );
        assert!(live.ages[liquid..].iter().all(|a| *a >= 0.0));
    }
}

/// The frame cap: the cap's frame count records (an empty frame is cheap), the next one is refused
/// with the protocol code, and the capped file still reads.
#[test]
fn frame_cap_is_enforced() {
    let s = FluidSim::new(FluidParams {
        bounds_max: Vec3::splat(0.5),
        cell_size: 0.25,
        ..FluidParams::default()
    })
    .expect("sim");
    let mut c = FluidCache::for_sim(&s, false);
    for _ in 0..MAX_FRAMES {
        c.record(&s).expect("within the cap");
    }
    let err = c.record(&s).expect_err("one over the cap");
    assert_eq!(err.code(), "fluidcache_too_large");
    assert_eq!(c.frame_count(), MAX_FRAMES as usize);
    let back = FluidCache::from_bytes(&c.to_bytes()).expect("capped file reads");
    assert_eq!(back.frame_count(), MAX_FRAMES as usize);
    assert_eq!(back.play(MAX_FRAMES as usize - 1).map(|f| f.len()), Ok(0));
    assert_eq!(
        back.play(MAX_FRAMES as usize).map_err(|e| e.code()),
        Err("fluidcache_no_frame")
    );
}

fn small_cache() -> (FluidCache, Vec<u8>) {
    let mut s = sim(true);
    let mut c = FluidCache::for_sim(&s, true);
    c.keyframe_interval = 2;
    for _ in 0..5 {
        s.step();
        c.record(&s).expect("record");
    }
    let b = c.to_bytes();
    (c, b)
}

fn set_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// Each header check of the reader fires with its own code, and none of them panics.
#[test]
fn header_corruptions_have_their_own_codes() {
    let (_, good) = small_cache();
    // Layout: magic 0..4, version 4, header_len 8, flags 12, frames 16, dt 20, origin 24..36, quantum 36,
    // keyframe 40, max_points 44, max_ww 48; frames from 52.
    let case = |name: &str, f: &dyn Fn(&mut Vec<u8>), want: &str| {
        let mut b = good.clone();
        f(&mut b);
        let got = FluidCache::from_bytes(&b).map(|_| ()).map_err(|e| e.code());
        assert_eq!(got, Err(want_static(want)), "{name}");
    };
    case(
        "header_len too small",
        &|b| set_u32(b, 8, 10),
        "fluidcache_corrupt",
    );
    case("unknown flag", &|b| set_u32(b, 12, 0x80), "fluidcache_corrupt");
    case(
        "dt NaN",
        &|b| b[20..24].copy_from_slice(&f32::NAN.to_le_bytes()),
        "fluidcache_corrupt",
    );
    case(
        "dt zero",
        &|b| b[20..24].copy_from_slice(&0.0f32.to_le_bytes()),
        "fluidcache_corrupt",
    );
    case(
        "origin inf",
        &|b| b[24..28].copy_from_slice(&f32::INFINITY.to_le_bytes()),
        "fluidcache_corrupt",
    );
    case(
        "quantum zero",
        &|b| b[36..40].copy_from_slice(&0.0f32.to_le_bytes()),
        "fluidcache_corrupt",
    );
    case(
        "quantum negative",
        &|b| b[36..40].copy_from_slice(&(-1.0f32).to_le_bytes()),
        "fluidcache_corrupt",
    );
    case(
        "keyframe interval zero",
        &|b| set_u32(b, 40, 0),
        "fluidcache_corrupt",
    );
    case(
        "max points over cap",
        &|b| set_u32(b, 44, 4_000_001),
        "fluidcache_too_large",
    );
    case(
        "max whitewater over cap",
        &|b| set_u32(b, 48, 4_000_001),
        "fluidcache_too_large",
    );
    case("max points lie", &|b| set_u32(b, 44, 1), "fluidcache_corrupt");
    case(
        "frame count too low leaves bytes",
        &|b| set_u32(b, 16, 4),
        "fluidcache_corrupt",
    );
    case("trailing garbage", &|b| b.push(0), "fluidcache_corrupt");
    case(
        "key flag contradicts the interval",
        &|b| set_u32(b, 40, 3),
        "fluidcache_corrupt",
    );
    case(
        "bad version",
        &|b| set_u32(b, 4, FLUIDCACHE_FORMAT_VERSION + 1),
        "fluidcache_bad_version",
    );
    case("empty", &|b| b.clear(), "fluidcache_truncated");
}

fn want_static(s: &str) -> &'static str {
    match s {
        "fluidcache_corrupt" => "fluidcache_corrupt",
        "fluidcache_too_large" => "fluidcache_too_large",
        "fluidcache_bad_version" => "fluidcache_bad_version",
        "fluidcache_truncated" => "fluidcache_truncated",
        other => panic!("unmapped code {other}"),
    }
}

/// A longer header than this build writes (a future minor addition) is skipped, and the file
/// re-encodes to the canonical v1 layout.
#[test]
fn longer_headers_are_skipped_and_cook_is_canonical() {
    let (c, good) = small_cache();
    let mut longer = good[..52].to_vec();
    longer.extend_from_slice(&[0xAB; 8]);
    longer.extend_from_slice(&good[52..]);
    set_u32(
        &mut longer,
        8,
        u32::from_le_bytes([good[8], good[9], good[10], good[11]]) + 8,
    );
    let read = FluidCache::from_bytes(&longer).expect("extended header reads");
    assert_eq!(read, c);
    assert_eq!(cook(&longer).expect("cook"), good, "cook writes canonical v1");
    assert_eq!(cook(&good).expect("cook"), good, "a v1 file re-encodes to itself");
    assert_eq!(cook(b"junk").map_err(|e| e.code()), Err("fluidcache_bad_magic"));
    assert_eq!(cook(b"RF").map_err(|e| e.code()), Err("fluidcache_truncated"));
}

/// The hostile-byte sweep runs against a deliberately tiny cache (8 cells, two frames): every
/// byte of the fixture is still flipped and the header keeps full coverage, but the sweep no
/// longer spends debug time fully decoding a multi-kilobyte whitewater run per flip.
fn tiny_cache_bytes() -> Vec<u8> {
    let mut params = FluidParams {
        bounds_min: Vec3::ZERO,
        bounds_max: Vec3::splat(0.25),
        cell_size: 0.125,
        ..FluidParams::default()
    };
    params.whitewater.enabled = true;
    let mut s = FluidSim::new(params).expect("sim");
    s.add_emitter(Emitter {
        shape: Shape::Box {
            center: Vec3::splat(0.125),
            half_extents: Vec3::splat(0.05),
        },
        velocity: Vec3::ZERO,
        kind: EmitterKind::Fill,
        done: false,
    })
    .expect("emitter");
    let mut c = FluidCache::for_sim(&s, true);
    c.keyframe_interval = 2;
    for _ in 0..2 {
        s.step();
        c.record(&s).expect("record");
    }
    c.to_bytes()
}

/// Corrupting any single byte never panics, and whatever still loads is stable under cook.
#[test]
fn single_byte_flips_never_panic_and_loads_are_stable() {
    let good = tiny_cache_bytes();
    let mut loaded = 0;
    for at in 0..good.len() {
        for mask in [0x01u8, 0x80, 0xFF] {
            let mut b = good.clone();
            b[at] ^= mask;
            if let Ok(c) = FluidCache::from_bytes(&b) {
                loaded += 1;
                let again = FluidCache::from_bytes(&c.to_bytes()).expect("a loaded cache re-reads");
                assert_eq!(again, c, "byte {at} mask {mask:#x}");
                for i in 0..c.frame_count() {
                    c.play(i).expect("every frame of a loaded cache plays");
                }
            }
        }
    }
    assert!(
        loaded > 0,
        "payload flips that stay valid exist (positions), so the loop exercised the Ok path"
    );
}

/// The probe, the fingerprint and the migration list.
#[test]
fn version_probe_fingerprint_and_migrations() {
    let (_, good) = small_cache();
    assert_eq!(&good[..4], &FLUIDCACHE_MAGIC);
    assert_eq!(
        fluidcache_file_version(&good),
        Ok(Some(FLUIDCACHE_FORMAT_VERSION))
    );
    assert!(fluidcache_file_version(&good[..7]).is_err());
    assert_eq!(fluidcache_file_version(b"RFLX____"), Ok(None));
    let mut future = good.clone();
    set_u32(&mut future, 4, 77);
    assert_eq!(fluidcache_file_version(&future), Ok(Some(77)));
    assert!(fluidcache_migration_sources().is_empty());
    let fp = fluidcache_schema_fingerprint();
    assert!(fp.starts_with("fluidcache: magic[4]=RFLC"), "{fp}");
    let c = FluidCache::from_bytes(&good).expect("read");
    assert_eq!(c.payload_bytes(), good.len() - 52 - 4 * c.frame_count());
    assert!(matches!(
        c.play(99),
        Err(CacheError::NoFrame { index: 99, count: 5 })
    ));
}
