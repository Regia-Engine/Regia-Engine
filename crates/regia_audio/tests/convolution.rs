// SPDX-License-Identifier: MPL-2.0
//! Convolution reverb acceptance (ADR-2721), CPU only, no audio device: the effect allocates
//! nothing while it processes, a crossfade between two IRs has no click, a bus convolution and a
//! zone IR render through the offline mixer, and refused IRs carry their codes.
// The counting allocator below forwards to the system allocator (the workspace denies unsafe
// code; test-only allocator shims are the documented exception, docs/04-conventions.md).
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use kira::Frame;
use regia_audio::{
    Audio, AudioSettings, BusEffect, BusLayout, CONVOLUTION_PARTITION, ConvolutionEffectBuilder,
    ConvolutionKernel, ConvolutionSlot, ConvolutionTier, ImpulseResponse, LoopRegion, PlaybackParams,
    ReverbMix, SoundAsset, SoundClip, ZONE_IR_BUS,
};

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

const RATE: u32 = 48_000;

fn noise(len: usize, seed: u32) -> Vec<f32> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        })
        .collect()
}

/// A decaying noise IR of `secs`.
fn room(secs: f32, seed: u32) -> ImpulseResponse {
    let len = (secs * RATE as f32) as usize;
    let taps: Vec<f32> = noise(len, seed)
        .into_iter()
        .enumerate()
        .map(|(i, s)| s * 0.3 * (-(i as f32) / (len as f32 / 5.0)).exp())
        .collect();
    ImpulseResponse::mono(taps, RATE).expect("a decaying noise IR is valid")
}

fn kernel(ir: &ImpulseResponse) -> Arc<ConvolutionKernel> {
    ConvolutionKernel::prepare(ir, RATE, ConvolutionTier::Desktop).expect("kernel")
}

fn sine_block(start: usize, len: usize, hz: f32) -> Vec<Frame> {
    (start..start + len)
        .map(|i| {
            let s = (std::f32::consts::TAU * hz * i as f32 / RATE as f32).sin() * 0.5;
            Frame { left: s, right: s }
        })
        .collect()
}

#[test]
fn process_allocates_nothing_over_ten_thousand_blocks() {
    let a = kernel(&room(0.5, 1));
    let b = kernel(&room(0.8, 2));
    let (builder, mut handle) =
        ConvolutionEffectBuilder::new(RATE, Some(ConvolutionSlot::new(a, 0.02)), 0.5, Some("a"));
    let mut effect = builder.into_effect();
    let info = kira::info::MockInfoBuilder::new().build();
    // kira hands effects blocks of its own size; use an odd one so blocks straddle partitions.
    const BLOCK: usize = 173;
    let mut buffer = vec![Frame::ZERO; BLOCK];
    let mut total = 0u64;
    for block in 0..10_000usize {
        if block == 3_000 {
            // The swap is built here, on the "game thread", outside the counted region.
            handle
                .set_kernel(Some("b"), Some(Arc::clone(&b)), 0.0, 0.25)
                .expect("swap queued");
        }
        if block == 6_000 {
            handle.set_mix(0.8);
            handle.set_kernel(None, None, 0.0, 0.1).expect("fade out queued");
        }
        buffer.copy_from_slice(&sine_block(block * BLOCK, BLOCK, 440.0));
        total += count_allocations(|| effect.process(&mut buffer, 1.0 / f64::from(RATE), &info));
        // The game thread frees what the audio thread retired.
        handle.collect();
    }
    assert_eq!(total, 0, "process allocated or freed {total} times");
    let stats = handle.stats();
    assert_eq!(stats.swaps, 2);
    assert_eq!(stats.crossfades_done, 2);
    assert!(stats.blocks >= (10_000 * BLOCK / CONVOLUTION_PARTITION) as u64 - 1);
}

/// The largest step between consecutive samples of a channel.
fn max_step(frames: &[Frame]) -> f32 {
    frames
        .windows(2)
        .map(|w| (w[1].left - w[0].left).abs().max((w[1].right - w[0].right).abs()))
        .fold(0.0, f32::max)
}

#[test]
fn a_crossfade_between_two_irs_has_no_step_above_the_steady_signals() {
    // Stated limit: during the crossfade no sample-to-sample step exceeds 1.25 x the largest
    // step either IR's steady output has on the same input (an equal-power fade of two
    // uncorrelated reverbs can sum to at most sqrt(2) of the larger, and a click would be far
    // above that).
    const LIMIT: f32 = 1.25;
    let a = kernel(&room(0.3, 3));
    let b = kernel(&room(0.6, 4));
    let render = |swap_at: Option<usize>, start: Arc<ConvolutionKernel>| {
        let (builder, mut handle) =
            ConvolutionEffectBuilder::new(RATE, Some(ConvolutionSlot::new(start, 0.0)), 1.0, None);
        let mut effect = builder.into_effect();
        let info = kira::info::MockInfoBuilder::new().build();
        let mut out = Vec::new();
        for block in 0..400usize {
            if Some(block) == swap_at {
                handle
                    .set_kernel(Some("b"), Some(Arc::clone(&b)), 0.0, 0.25)
                    .expect("swap");
            }
            let mut buf = sine_block(block * 128, 128, 220.0);
            effect.process(&mut buf, 1.0 / 48_000.0, &info);
            out.extend(buf);
        }
        (out, handle.stats())
    };
    let (steady_a, _) = render(None, Arc::clone(&a));
    let (steady_b, _) = render(None, Arc::clone(&b));
    let (faded, stats) = render(Some(150), a);
    // Compare after both reverbs have filled (0.6 s of input).
    let settle = 150 * 128;
    let reference = max_step(&steady_a[settle..]).max(max_step(&steady_b[settle..]));
    let during = max_step(&faded[settle..]);
    assert!(reference > 0.0);
    assert!(
        during <= LIMIT * reference,
        "crossfade step {during} vs steady {reference} (limit {LIMIT}x)"
    );
    assert_eq!(stats.crossfades_done, 1);
}

fn tone(hz: f32, secs: f32) -> SoundAsset {
    let samples: Vec<f32> = (0..(secs * RATE as f32) as usize)
        .map(|i| (std::f32::consts::TAU * hz * i as f32 / RATE as f32).sin() * 0.25)
        .collect();
    SoundAsset::from_clip(&SoundClip::new(samples, RATE).expect("tone"))
}

fn looping(bus: &str) -> PlaybackParams {
    PlaybackParams {
        bus: Some(bus.into()),
        loop_region: Some(LoopRegion {
            start_secs: 0.0,
            end_secs: None,
        }),
        ..PlaybackParams::default()
    }
}

fn headless(buses: BusLayout) -> Audio {
    Audio::new(&AudioSettings {
        headless: true,
        buses,
        ..AudioSettings::default()
    })
}

fn render(audio: &mut Audio, ticks: usize) -> Vec<f32> {
    let mut out = Vec::new();
    for _ in 0..ticks {
        audio.tick(0.01);
        out.extend(audio.render_offline(480).expect("offline backend"));
    }
    out
}

fn energy(samples: &[f32]) -> f64 {
    samples.iter().map(|s| f64::from(*s).powi(2)).sum()
}

#[test]
fn a_bus_convolution_renders_its_ir_and_an_unknown_ir_passes_dry() {
    // A send bus convolution fed by World at 0 dB.
    let mut layout = BusLayout::default_tree();
    if let Some(send) = layout.send_buses.iter_mut().find(|s| s.name == "Reverb") {
        send.effects = vec![BusEffect::Convolution {
            ir: "hall".into(),
            mix: 1.0,
            predelay_secs: 0.0,
        }];
    }
    for bus in &mut layout.buses {
        for send in &mut bus.sends {
            send.volume_db = 0.0;
        }
    }
    let mut audio = headless(layout.clone());
    // Before the IR is registered the reverb bus is silent: only the dry World signal plays.
    let asset = tone(330.0, 1.0);
    audio.start_asset(&asset, looping("World")).expect("play");
    let dry = render(&mut audio, 50);
    audio
        .register_impulse_response("hall", room(0.4, 9))
        .expect("register");
    let wet = render(&mut audio, 80);
    let stats = audio.convolution_stats();
    let reverb = stats
        .iter()
        .find(|r| r.bus == "Reverb")
        .expect("the reverb convolution reports");
    assert_eq!(reverb.ir.as_deref(), Some("hall"));
    assert_eq!(reverb.stats.ir_frames, (0.4 * RATE as f32) as usize);
    assert!(reverb.stats.blocks > 0);
    assert!(!reverb.stats.rate_mismatch);
    // The reverb adds energy once the IR plays.
    let tail = &wet[wet.len() / 2..];
    let before = &dry[dry.len() / 2..];
    assert!(
        energy(tail) > energy(before) * 1.05,
        "{} vs {}",
        energy(tail),
        energy(before)
    );
    // Refusals: an unknown IR and a NaN mix.
    assert!(
        audio
            .set_bus_convolution("Reverb", 0, Some("nope"), 0.0, 0.1)
            .is_err()
    );
    assert!(audio.set_bus_convolution_mix("Reverb", 0, f32::NAN).is_err());
    assert!(audio.set_bus_convolution("World", 0, None, 0.0, 0.1).is_err());
}

#[test]
fn a_zone_ir_crossfades_in_without_a_click_and_low_tier_keeps_the_preset() {
    let mut audio = headless(BusLayout::default_tree());
    audio
        .register_impulse_response("cave", room(0.5, 21))
        .expect("register");
    let asset = tone(220.0, 1.0);
    audio.start_asset(&asset, looping("World")).expect("play");
    let zone = ReverbMix {
        weight: 1.0,
        send_db: -3.0,
        ..ReverbMix::DRY
    };
    audio.set_reverb_mix(Some(zone));
    let preset = render(&mut audio, 60);
    // Into the IR zone: the sends crossfade from the preset reverb to the zone convolver.
    audio.set_reverb_zone_ir(Some("cave"));
    audio.set_reverb_mix(Some(zone));
    let switched = render(&mut audio, 100);
    assert_eq!(audio.reverb_zone_ir(), Some("cave"));
    let zone_stats = audio
        .convolution_stats()
        .into_iter()
        .find(|r| r.bus == ZONE_IR_BUS)
        .expect("zone convolver");
    assert_eq!(zone_stats.ir.as_deref(), Some("cave"));
    assert!(zone_stats.stats.blocks > 0);
    // No click: the largest step in the switch window (the first 0.4 s; the sends fade over
    // 0.25 s) stays within 1.25x the larger of the two steady mixes' (preset before, IR after;
    // the noise IR is broadband, so its own steady steps are larger than the preset's).
    let frames = |s: &[f32]| -> Vec<Frame> {
        s.chunks(2)
            .map(|c| Frame {
                left: c[0],
                right: c.get(1).copied().unwrap_or(c[0]),
            })
            .collect()
    };
    let window = 40 * 480 * 2;
    let steady_preset = max_step(&frames(&preset[preset.len() / 2..]));
    let steady_ir = max_step(&frames(&switched[switched.len() / 2..]));
    let steady = steady_preset.max(steady_ir);
    let across = max_step(&frames(&switched[..window]));
    assert!(
        across <= 1.25 * steady,
        "switch step {across} vs steady {steady} (preset {steady_preset}, IR {steady_ir})"
    );

    // The low tier never loads a zone IR: the preset stays.
    let mut low = headless(BusLayout::default_tree());
    low.set_convolution_tier(ConvolutionTier::Low);
    assert!(low.register_impulse_response("cave", room(0.5, 21)).is_err());
    // The sync asks every frame: one fallback per change of request, not per frame.
    for _ in 0..10 {
        low.set_reverb_zone_ir(Some("cave"));
    }
    assert_eq!(low.reverb_zone_ir(), None);
    assert_eq!(low.reverb_zone_ir_counts().1, 1, "one fallback counted");
}

#[test]
fn refused_irs_carry_documented_codes_and_never_panic() {
    let mut audio = headless(BusLayout::default_tree());
    let empty = ImpulseResponse::from_encoded_bytes(&[]).expect_err("empty");
    assert_eq!(empty.code(), "ir_empty");
    // A WAV cut in its data chunk.
    let clip = SoundClip::new(noise(4_800, 5), RATE).expect("clip");
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    let data_len = (clip.len() * 2) as u32;
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    for v in [1u16, 1] {
        wav.extend_from_slice(&v.to_le_bytes());
    }
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 2).to_le_bytes());
    for v in [2u16, 16] {
        wav.extend_from_slice(&v.to_le_bytes());
    }
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for s in clip.samples() {
        wav.extend_from_slice(&((s * 32_000.0) as i16).to_le_bytes());
    }
    let whole = ImpulseResponse::from_encoded_bytes(&wav).expect("the whole WAV decodes");
    assert_eq!(whole.frames(), 4_800);
    for cut in [wav.len() - 1, wav.len() / 2, 44, 20, 3] {
        let error = ImpulseResponse::from_encoded_bytes(&wav[..cut]).expect_err("cut");
        assert!(
            matches!(error.code(), "ir_truncated" | "ir_empty"),
            "{cut}: {error}"
        );
    }
    // Too long for the mobile tier.
    audio.set_convolution_tier(ConvolutionTier::Mobile);
    let long = audio
        .register_impulse_response("long", room(2.0, 1))
        .expect_err("2 s is over the 1.5 s mobile cap");
    assert_eq!(long.code(), "ir_too_long");
    assert!(audio.impulse_responses().is_empty());
}
