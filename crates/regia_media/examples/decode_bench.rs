// SPDX-License-Identifier: MPL-2.0
//! Decoder benchmark (ADR-1670, ADR-1869): `decode_bench [file.webm] [frames]`. Without a file it decodes a
//! synthetic 1080p raw clip. Prints one RON line per run. With `DECODE_BENCH_AUDIO=1` it times the default
//! audio track instead (milliseconds per 20 ms of audio).

use regia_media::bench;
use regia_media::mux::PatternClip;

fn main() {
    let mut args = std::env::args().skip(1);
    let file = args.next();
    let frames: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(120);
    let bytes = match &file {
        Some(path) => match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("cannot read {path}: {e}");
                std::process::exit(2);
            }
        },
        None => PatternClip::new(1920, 1080, frames.min(240) as u32, 30).to_webm(),
    };
    if std::env::var_os("DECODE_BENCH_AUDIO").is_some() {
        match bench::run_audio(bytes) {
            Ok(report) => match ron::ser::to_string(&report) {
                Ok(line) => println!("{line}"),
                Err(_) => println!("{report:?}"),
            },
            Err(e) => {
                eprintln!("decode_bench: {} ({e})", e.code());
                std::process::exit(1);
            }
        }
        return;
    }
    match bench::run(bytes, frames, true) {
        Ok(report) => match serde_json_line(&report) {
            Some(line) => println!("{line}"),
            None => println!("{report:?}"),
        },
        Err(e) => {
            eprintln!("decode_bench: {} ({e})", e.code());
            std::process::exit(1);
        }
    }
}

fn serde_json_line(r: &bench::BenchReport) -> Option<String> {
    ron::ser::to_string(r).ok()
}
