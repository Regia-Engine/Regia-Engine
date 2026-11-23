// SPDX-License-Identifier: MPL-2.0
//! Hand-run timing of the voice codec against the budgets of specs/59 § 10.
//!
//! `cargo run --release -p regia_media --features voice-opus --example voice_bench`
//!
//! Prints the median encode, decode and concealment time per 20 ms frame at each profile, complexity 5.

use regia_audio::voice_chat::{MAX_PAYLOAD, VoiceCodec, VoiceQuality};
use regia_media::voice_opus::OpusVoiceCodec;
use std::time::Instant;

const FRAMES: usize = 500;

fn speech(rate: u32) -> Vec<f32> {
    let n = rate as usize / 50 * FRAMES;
    (0..n)
        .map(|i| {
            let t = i as f64 / f64::from(rate);
            let envelope = 0.55 + 0.45 * (std::f64::consts::TAU * 4.0 * t).sin();
            let voice: f64 = (1..=12)
                .map(|h| {
                    let h = f64::from(h);
                    (std::f64::consts::TAU * 140.0 * h * t).sin() / h
                })
                .sum();
            (voice * envelope * 0.12) as f32
        })
        .collect()
}

fn median_us(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("profile   encode_us decode_us conceal_us mean_bytes");
    for quality in [VoiceQuality::Narrow, VoiceQuality::Standard, VoiceQuality::High] {
        let frame = quality.frame_samples();
        let input = speech(quality.sample_rate());
        let mut tx = OpusVoiceCodec::new(quality, 5)?;
        let mut rx = OpusVoiceCodec::new(quality, 5)?;
        let mut packets = Vec::with_capacity(FRAMES);
        let mut encode = Vec::with_capacity(FRAMES);
        for f in input.chunks_exact(frame) {
            let mut out = vec![0u8; MAX_PAYLOAD];
            let start = Instant::now();
            let n = tx.encode(f, &mut out)?;
            encode.push(start.elapsed().as_secs_f64() * 1e6);
            out.truncate(n);
            packets.push(out);
        }
        let mut pcm = vec![0.0f32; frame];
        let mut decode = Vec::with_capacity(FRAMES);
        for p in &packets {
            let start = Instant::now();
            rx.decode(Some(p), false, &mut pcm)?;
            decode.push(start.elapsed().as_secs_f64() * 1e6);
        }
        let mut conceal = Vec::new();
        for p in packets.iter().step_by(10) {
            rx.decode(Some(p), false, &mut pcm)?;
            let start = Instant::now();
            rx.decode(None, false, &mut pcm)?;
            conceal.push(start.elapsed().as_secs_f64() * 1e6);
        }
        let bytes = packets.iter().map(Vec::len).sum::<usize>() as f64 / packets.len() as f64;
        println!(
            "{:9} {:9.1} {:9.1} {:10.1} {:10.1}",
            quality.name(),
            median_us(encode),
            median_us(decode),
            median_us(conceal),
            bytes
        );
    }
    Ok(())
}
