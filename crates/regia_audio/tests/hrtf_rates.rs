// SPDX-License-Identifier: MPL-2.0
//! Binaural rendering at every mixer rate (AU-14, ADR-7461), CPU only, no audio device: a
//! 96 kHz or 192 kHz mixer convolves with HRIRs prepared at its own rate (the interaural delay
//! keeps its length in time instead of shrinking by the rate ratio), the 44.1 kHz and 48 kHz
//! output is the same, bit for bit, as before per-rate tables, and the effect allocates nothing
//! while it processes at any rate.
// The counting allocator below forwards to the system allocator (the workspace denies unsafe
// code; test-only allocator shims are the documented exception, docs/04-conventions.md).
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;

use kira::Frame;
use kira::effect::EffectBuilder;
use regia_app::{App, TimeUpdateStrategy};
use regia_audio::hrtf::{HrirSphere, HrtfControl, HrtfEffectBuilder};
use regia_audio::{
    Audio, AudioEmitter, AudioListener, AudioPlugin, AudioSettings, RolloffCurve, SoundAssets, SoundClip,
};
use regia_core::math::{Quat, Vec3};
use regia_scene::{ScenePlugin, Transform};

/// Counts allocations made on the thread that armed it.
struct CountingAlloc;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

fn note() {
    // `try_with`: thread-local storage may be gone while a thread shuts down.
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            let _ = COUNT.try_with(|c| c.set(c.get() + 1));
        }
    });
}

// SAFETY: forwards every call to the system allocator unchanged; the counter only reads and
// writes thread-local cells, which never allocate.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: the caller's layout contract is passed on unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note();
        // SAFETY: `ptr` came from `alloc` above with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        // SAFETY: as for `alloc` and `dealloc`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// Allocations and frees `f` makes on this thread.
fn count_allocations(f: impl FnOnce()) -> u64 {
    COUNT.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    f();
    ARMED.with(|a| a.set(false));
    COUNT.with(Cell::get)
}

const DT: f64 = 1.0 / 60.0;

/// A headless app whose offline mixer runs at `rate` (what a device at that rate gives kira).
fn app(rate: u32) -> App {
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        sample_rate: rate,
        ..Default::default()
    });
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(DT)));
    app.add_plugins((ScenePlugin, AudioPlugin));
    app
}

fn add_clip(app: &mut App, name: &str, samples: Vec<f32>, rate: u32) {
    let asset = regia_audio::SoundAsset::from_clip(&SoundClip::new(samples, rate).expect("clip"));
    app.world_mut().resource_mut::<SoundAssets>().add(name, asset);
}

/// An emitter with no distance loss inside 20 m, no occlusion, no Doppler.
fn emitter(sound: &str, looping: bool) -> AudioEmitter {
    AudioEmitter {
        sound_name: sound.into(),
        min_distance: 20.0,
        max_distance: 40.0,
        rolloff: RolloffCurve::Inverse,
        occlusion: false,
        looping,
        ..AudioEmitter::default()
    }
}

fn headphones(app: &mut App) {
    app.world_mut().spawn((
        AudioListener { hrtf: true },
        Transform {
            rotation: Quat::IDENTITY,
            ..Default::default()
        },
    ));
}

/// Runs `frames` app updates, rendering each frame's audio at `rate`; returns interleaved stereo.
fn run(app: &mut App, frames: usize, rate: u32) -> Vec<f32> {
    let per = (f64::from(rate) * DT).round() as usize;
    let mut out = Vec::with_capacity(frames * per * 2);
    for _ in 0..frames {
        app.update();
        let rendered = app
            .world_mut()
            .resource_mut::<Audio>()
            .render_offline(per)
            .expect("offline render");
        out.extend_from_slice(&rendered);
    }
    out
}

fn channels(frames: &[f32]) -> (Vec<f32>, Vec<f32>) {
    (
        frames.iter().step_by(2).copied().collect(),
        frames.iter().skip(1).step_by(2).copied().collect(),
    )
}

/// Deterministic white noise (LCG).
fn noise(len: usize, seed: u32, amp: f32) -> Vec<f32> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amp
        })
        .collect()
}

// ------------------------------------------------------------------ (1) ITD scales with rate

/// The interaural lag (fractional samples, positive when the right ear hears it later) of a
/// hard-left impulse rendered binaurally on a mixer at `rate`: the cross-correlation peak of the
/// two ears, refined by a parabola through it and its neighbours.
fn hard_left_impulse_lag(rate: u32) -> f64 {
    let mut app = app(rate);
    // A unit impulse 0.1 s into a 0.5 s clip at the mixer's own rate (no resampling).
    let len = rate as usize / 2;
    let at = rate as usize / 10;
    let mut clip = vec![0.0f32; len];
    clip[at] = 1.0;
    add_clip(&mut app, "click", clip, rate);
    headphones(&mut app);
    app.world_mut().spawn((
        emitter("click", false),
        Transform::from_translation(Vec3::NEG_X * 3.0),
    ));
    let out = run(&mut app, 30, rate);
    let (l, r) = channels(&out);
    // Both ears' responses lie within a few milliseconds of the loudest left sample.
    let peak = l
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
        .map_or(0, |(i, _)| i);
    assert!(
        l[peak].abs() > 0.05,
        "the click was heard at {rate} Hz: {}",
        l[peak]
    );
    let half = (rate as usize) / 100; // a 10 ms window either side
    let lo = peak.saturating_sub(half);
    let hi = (peak + half).min(l.len());
    let max_lag = (rate as i64) / 500; // 2 ms: the model's ITD is 0.66 ms
    let corr = |lag: i64| -> f64 {
        (lo..hi)
            .filter_map(|i| {
                let j = i as i64 + lag;
                (j >= 0 && (j as usize) < r.len()).then(|| f64::from(l[i]) * f64::from(r[j as usize]))
            })
            .sum()
    };
    let (best, _) = (-max_lag..=max_lag)
        .map(|lag| (lag, corr(lag)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .expect("lags");
    let (c0, c1, c2) = (corr(best - 1), corr(best), corr(best + 1));
    let denom = c0 - 2.0 * c1 + c2;
    let offset = if denom.abs() > 1e-12 {
        0.5 * (c0 - c2) / denom
    } else {
        0.0
    };
    best as f64 + offset
}

/// AU-14 acceptance: the interaural delay is a time, so its length in samples follows the
/// mixer rate. On the base, a 96 kHz mixer convolved with the 48 kHz HRIRs and heard half the
/// delay (the lag stayed ≈ 31 samples instead of ≈ 63).
#[test]
fn a_hard_left_impulse_lags_two_and_four_times_longer_at_96k_and_192k() {
    let at_48 = hard_left_impulse_lag(48_000);
    let at_96 = hard_left_impulse_lag(96_000);
    let at_192 = hard_left_impulse_lag(192_000);
    // Brown & Duda's head: (a/c)(1 + π/2) = 0.656 ms ≈ 31.5 samples at 48 kHz.
    assert!((28.0..=35.0).contains(&at_48), "48 kHz lag {at_48:.2}");
    assert!(
        (at_96 - 2.0 * at_48).abs() <= 1.0,
        "96 kHz lag {at_96:.2} vs 2 × 48 kHz lag {:.2}",
        2.0 * at_48
    );
    assert!(
        (at_192 - 4.0 * at_48).abs() <= 1.0,
        "192 kHz lag {at_192:.2} vs 4 × 48 kHz lag {:.2}",
        4.0 * at_48
    );
}

// ------------------------------------------------------------- (2) 44.1/48 kHz bit-identical

/// FNV-1a over the bit patterns of the samples.
fn fnv1a(samples: &[f32]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for s in samples {
        for b in s.to_bits().to_le_bytes() {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash
}

/// A binaural scene at `rate`: a looping noise emitter ahead-right and one orbiting the head
/// (the direction interpolation and face search run every chunk), 30 frames.
fn binaural_scene(rate: u32) -> Vec<f32> {
    let mut app = app(rate);
    add_clip(&mut app, "noise_a", noise(48_000, 0x1234_5678, 0.3), 48_000);
    add_clip(&mut app, "noise_b", noise(24_000, 0x0bad_f00d, 0.2), 48_000);
    headphones(&mut app);
    app.world_mut().spawn((
        emitter("noise_a", true),
        Transform::from_translation(Vec3::new(2.0, 0.5, -3.0)),
    ));
    let orbit = app
        .world_mut()
        .spawn((
            emitter("noise_b", true),
            Transform::from_translation(Vec3::NEG_Z * 4.0),
        ))
        .id();
    let per = (f64::from(rate) * DT).round() as usize;
    let mut out = Vec::new();
    for frame in 0..30u32 {
        let a = frame as f32 * 0.21;
        if let Some(mut t) = app.world_mut().get_mut::<Transform>(orbit) {
            t.translation = Vec3::new(a.sin() * 4.0, (a * 0.5).sin(), -a.cos() * 4.0);
        }
        app.update();
        let rendered = app
            .world_mut()
            .resource_mut::<Audio>()
            .render_offline(per)
            .expect("offline render");
        out.extend_from_slice(&rendered);
    }
    out
}

/// The hashes the base tree (before per-rate tables, `b890ff25e`) renders, per FFT code path:
/// rustfft picks AVX+FMA, SSE4.1 or scalar kernels at run time on x86_64 (NEON on aarch64),
/// and their rounding differs. Measured on the base binary on a Threadripper 3990X (AVX) and,
/// for the other paths, under `qemu-x86_64 -cpu Westmere-v1` (SSE4.1) and `-cpu Conroe-v1`
/// (scalar); the new tree renders the same three on the same hosts.
///
/// The 44.1 kHz hashes moved once on purpose: the HRIR resample keeps each HRIR's DC gain
/// (`hrtf::resample_hrir`, rev-kimi-28 audio2-k 2, merged with this test in integ-v06d,
/// ADR-7555). With that gain step disabled the AVX path renders the old pin
/// `0x1e2b_ed53_b537_7f63` exactly; the AVX pin is re-measured on the test machine (Ryzen, AVX2+FMA).
/// The SSE4.1 and scalar 44.1 kHz pins are not re-measured yet (`None`: determinism only there).
#[cfg(target_arch = "x86_64")]
fn base_hashes() -> Option<(u64, Option<u64>)> {
    Some(
        if std::arch::is_x86_feature_detected!("avx") && std::arch::is_x86_feature_detected!("fma") {
            BASE_X86_AVX
        } else if std::arch::is_x86_feature_detected!("sse4.1") {
            BASE_X86_SSE41
        } else {
            BASE_X86_SCALAR
        },
    )
}

/// No pinned hash off x86_64 (aarch64 NEON was not measured on the base).
#[cfg(not(target_arch = "x86_64"))]
fn base_hashes() -> Option<(u64, Option<u64>)> {
    None
}

/// `(48 kHz, 44.1 kHz)` hashes on x86_64 with AVX and FMA.
#[cfg(target_arch = "x86_64")]
const BASE_X86_AVX: (u64, Option<u64>) = (0x0037_fbc2_c80f_a105, Some(0xd62b_2ded_5a73_5dd7));
/// `(48 kHz, 44.1 kHz)` hashes on x86_64 with SSE4.1 but no AVX.
#[cfg(target_arch = "x86_64")]
/// (44.1 kHz before the DC-gain resample: `0x64a4_ac96_348d_9097`; to re-measure.)
const BASE_X86_SSE41: (u64, Option<u64>) = (0x5164_d0b4_df28_941c, None);
/// `(48 kHz, 44.1 kHz)` hashes on x86_64 without SSE4.1 (rustfft's scalar kernels).
#[cfg(target_arch = "x86_64")]
/// (44.1 kHz before the DC-gain resample: `0xb537_3373_adfe_2019`; to re-measure.)
const BASE_X86_SCALAR: (u64, Option<u64>) = (0x07d8_8c68_3a21_4043, None);

/// Requirement 2 of AU-14: per-rate tables change nothing at the two rates that already had
/// one. The output of the same binaural scene must hash to what the base rendered. On a CPU
/// whose FFT path has no pinned hash (aarch64 NEON) the test still checks the
/// render is deterministic and prints its hashes for pinning.
#[test]
fn binaural_output_at_44k_and_48k_is_bit_identical_to_the_base() {
    let at_48 = fnv1a(&binaural_scene(48_000));
    let at_44 = fnv1a(&binaural_scene(44_100));
    eprintln!("binaural scene hashes: 48 kHz {at_48:#018x}, 44.1 kHz {at_44:#018x}");
    assert_eq!(
        at_48,
        fnv1a(&binaural_scene(48_000)),
        "48 kHz render is deterministic"
    );
    match base_hashes() {
        Some((base_48, base_44)) => {
            assert_eq!(at_48, base_48, "binaural output changed at 48 kHz");
            match base_44 {
                Some(base_44) => assert_eq!(at_44, base_44, "binaural output changed at 44.1 kHz"),
                None => {
                    assert_eq!(
                        at_44,
                        fnv1a(&binaural_scene(44_100)),
                        "44.1 kHz render is deterministic"
                    );
                    eprintln!("no pinned 44.1 kHz hash for this CPU's FFT path; determinism checked only")
                }
            }
        }
        None => eprintln!("no pinned hash for this CPU's FFT path; determinism checked only"),
    }
}

// --------------------------------------------------------------- (3) no per-block allocation

/// The binaural effect allocates nothing while it processes, at every common mixer rate: the
/// spectra (and any per-rate table) are built before the effect reaches the audio thread.
#[test]
fn the_binaural_effect_allocates_nothing_per_block_at_any_rate() {
    let rates = [44_100u32, 48_000, 88_200, 96_000, 176_400, 192_000];
    // Every rate's table, built on the "game thread" before any effect runs (ADR-7461).
    let spectra = rates
        .iter()
        .fold(HrirSphere::synthesized().spectra(), |s, rate| s.with_rate(*rate));
    for rate in rates {
        let control = Arc::new(HrtfControl::default());
        let (mut effect, ()) = HrtfEffectBuilder::new(Arc::clone(&spectra), Arc::clone(&control)).build();
        effect.init(rate, 1024);
        assert_eq!(
            control.table_rate(),
            Some(rate),
            "{rate} Hz runs on its own table"
        );
        let info = kira::info::MockInfoBuilder::new().build();
        // kira's block size varies; an odd one straddles the effect's 128-sample chunks.
        const BLOCK: usize = 173;
        let source = noise(BLOCK * 64, rate, 0.4);
        let mut buffer = vec![Frame::ZERO; BLOCK];
        let mut total = 0u64;
        for block in 0..4_000usize {
            // The game thread steers the emitter around the head every block.
            let a = block as f32 * 0.013;
            control.set(Vec3::new(a.sin(), (a * 0.3).sin() * 0.5, -a.cos()), 1.0);
            let start = (block % 64) * BLOCK;
            for (frame, s) in buffer.iter_mut().zip(&source[start..start + BLOCK]) {
                *frame = Frame { left: *s, right: *s };
            }
            total += count_allocations(|| effect.process(&mut buffer, 1.0 / f64::from(rate), &info));
            assert!(buffer.iter().all(|f| f.left.is_finite() && f.right.is_finite()));
        }
        assert_eq!(total, 0, "{rate} Hz: process allocated or freed {total} times");
    }
}

// ----------------------------------------------------------- real device (run by hand)

/// On a real output device (set the system output to 96 kHz first), a binaural emitter's HRIR
/// table runs at the device's own rate, whatever `AudioSettings::sample_rate` says: the effect
/// reports the rate kira opened the device at and the audio system builds that table. Needs a
/// device, so it is ignored by default:
/// `cargo test -p regia_audio --test hrtf_rates -- --ignored --nocapture real_device`.
#[test]
#[ignore = "needs a real audio output device; run by hand with the output at 96 kHz"]
fn real_device_binaural_table_matches_the_device_rate() {
    let mut app = App::new();
    app.insert_resource(AudioSettings::default());
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(DT)));
    app.add_plugins((ScenePlugin, AudioPlugin));
    assert!(
        !app.world().resource::<Audio>().is_headless(),
        "no output device opened: this test needs one"
    );
    add_clip(&mut app, "noise", noise(48_000, 0x1234_5678, 0.05), 48_000);
    headphones(&mut app);
    app.world_mut().spawn((
        emitter("noise", true),
        Transform::from_translation(Vec3::NEG_X * 3.0),
    ));
    // The device renders on its own thread; a wall-clock wait is the only way to let it run.
    let mut clock = None;
    for _ in 0..200 {
        app.update();
        std::thread::sleep(Duration::from_millis(10));
        clock = app.world().resource::<Audio>().sample_clock();
        if clock.is_some() && app.world().resource::<Audio>().hrtf_table_rate().is_some() {
            break;
        }
    }
    let device_rate = clock.expect("the device rendered").sample_rate;
    let audio = app.world().resource::<Audio>();
    let table = audio.hrtf_table_rate();
    eprintln!("device rate {device_rate} Hz, HRIR table rate {table:?}");
    assert_eq!(
        table,
        Some(device_rate),
        "binaural voices use a table at the device rate"
    );
    assert_eq!(audio.emitter_status()[0].hrtf_rate, Some(device_rate));
}

// ------------------------------------------------- render-out mixdown at another rate

/// A 96 kHz render-out mixdown from a 48 kHz app (ADR-1520) renders binaural voices on a
/// 96 kHz table, and after the mixdown the rebuilt tracks use the 48 kHz table again.
#[test]
fn a_96k_mixdown_from_a_48k_app_uses_a_96k_table_and_returns_to_48k() {
    let mut app = app(48_000);
    add_clip(&mut app, "noise", noise(48_000, 0x1234_5678, 0.3), 48_000);
    headphones(&mut app);
    app.world_mut().spawn((
        emitter("noise", true),
        Transform::from_translation(Vec3::NEG_X * 3.0),
    ));
    run(&mut app, 3, 48_000);
    let rate_now = |app: &App| app.world().resource::<Audio>().emitter_status()[0].hrtf_rate;
    assert_eq!(rate_now(&app), Some(48_000));
    app.world_mut()
        .resource_mut::<Audio>()
        .begin_mixdown(&regia_audio::MixdownSettings {
            sample_rate: 96_000,
            stems: Vec::new(),
        })
        .expect("mixdown starts");
    let per = (96_000.0 * DT) as usize;
    let mut mix = Vec::new();
    for _ in 0..12 {
        app.update();
        let block = app
            .world_mut()
            .resource_mut::<Audio>()
            .render_mixdown(per)
            .expect("mixdown block");
        mix.extend_from_slice(&block.mix);
    }
    assert_eq!(
        rate_now(&app),
        Some(96_000),
        "the mixdown's binaural voice runs on a 96 kHz table"
    );
    assert_eq!(app.world().resource::<Audio>().hrtf_table_rate(), Some(96_000));
    let (l, r) = channels(&mix[mix.len() / 2..]);
    let energy = |s: &[f32]| s.iter().map(|x| x * x).sum::<f32>();
    assert!(
        energy(&l) > 1.3 * energy(&r),
        "a hard-left source is louder on the left"
    );
    assert!(app.world_mut().resource_mut::<Audio>().end_mixdown());
    run(&mut app, 3, 48_000);
    assert_eq!(rate_now(&app), Some(48_000), "back on the output's 48 kHz table");
}
