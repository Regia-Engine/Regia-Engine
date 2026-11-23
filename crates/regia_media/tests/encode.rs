// SPDX-License-Identifier: MPL-2.0
//! The offline WebM encoder (spec 50 § 14, ADR-3369) read back through Regia's own demuxer and
//! decoders. CPU only, small synthetic pictures from the fixture generator (`mux::pattern_frame`),
//! no goldens: pictures are compared to their source by PSNR, sound by length and level.
#![cfg(all(feature = "media-encode", feature = "av1", feature = "opus"))]

use std::io::Cursor;
use std::path::{Path, PathBuf};

use regia_media::codec;
use regia_media::container::{Demuxer, MkvDemuxer, TrackKind};
use regia_media::encode::wav::Pcm;
use regia_media::encode::webm::{AUDIO_TRACK, VIDEO_TRACK, WebmEncoder};
use regia_media::encode::{EncodeSettings, frame_time_ns, psnr};
use regia_media::mux::pattern_frame;

const W: u32 = 128;
const H: u32 = 72;
const FRAMES: u32 = 48;
const FPS: u32 = 24;
/// The quality the PSNR floor is stated at (the AV1 quantizer; the default is 80).
const QUALITY: u8 = 80;
/// Worst-picture luma PSNR floor at [`QUALITY`], speed 10, for the 48-frame 128x72 pattern clip, dB.
/// Measured 44.81 dB (mean 47.69) on the first run (the build server, 2026-11-06); the floor leaves margin for
/// encoder updates, never for a broken pipeline (a shifted or swapped plane scores under 15 dB).
const PSNR_FLOOR_DB: f64 = 40.0;

fn temp_dir(tag: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-encode-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn settings(threads: u32) -> EncodeSettings {
    EncodeSettings {
        quality: QUALITY,
        speed: 10,
        keyframe_interval: 24,
        threads,
        ..EncodeSettings::default()
    }
}

fn tone(rate: u32, channels: u16, frames: u64) -> Pcm {
    let samples = (0..frames)
        .flat_map(|i| {
            let v = (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 0.5;
            std::iter::repeat_n(v, usize::from(channels))
        })
        .collect();
    Pcm {
        rate,
        channels,
        samples,
    }
}

fn encode(path: &Path, settings: &EncodeSettings, audio: Option<&Pcm>) -> regia_media::encode::EncodeReport {
    let mut enc = WebmEncoder::create(path, W, H, FPS, 1, settings).expect("encoder");
    for k in 0..FRAMES {
        enc.push_i420(&pattern_frame(W, H, k)).expect("picture");
    }
    enc.finish(audio).expect("finish")
}

struct ReadBack {
    pictures: Vec<regia_media::codec::DecodedPicture>,
    video_pts: Vec<i64>,
    keyframes: Vec<bool>,
    audio: Vec<f32>,
    audio_channels: usize,
    duration_ns: Option<u64>,
    cues: usize,
}

fn read_back(bytes: Vec<u8>) -> ReadBack {
    let mut d = MkvDemuxer::open(Cursor::new(bytes)).expect("demux");
    let info = d.probe().expect("probe");
    let vt = info.video_track().expect("video track").clone();
    assert_eq!(vt.codec_id, codec::AV1);
    assert_eq!(vt.number, VIDEO_TRACK);
    let mut video = codec::video_decoder_for(&vt).expect("av1 decoder");
    let at = info.tracks.iter().find(|t| t.kind == TrackKind::Audio).cloned();
    let mut audio_dec = at
        .as_ref()
        .map(|t| codec::audio_decoder_for(t).expect("opus decoder"));
    let mut out = ReadBack {
        pictures: Vec::new(),
        video_pts: Vec::new(),
        keyframes: Vec::new(),
        audio: Vec::new(),
        audio_channels: at
            .as_ref()
            .and_then(|t| t.audio.as_ref())
            .map_or(0, |a| usize::from(a.channels)),
        duration_ns: info.duration_ns,
        cues: info.cue_count,
    };
    while let Some(p) = d.next_packet().expect("packet") {
        if p.track == VIDEO_TRACK {
            out.video_pts.push(p.pts_ns);
            out.keyframes.push(p.keyframe);
            out.pictures.extend(video.decode(&p).expect("decode"));
        } else if p.track == AUDIO_TRACK {
            audio_dec
                .as_mut()
                .expect("audio")
                .decode(&p, &mut out.audio)
                .expect("decode audio");
        }
    }
    out.pictures.extend(video.drain().expect("drain"));
    out
}

#[test]
fn pictures_round_trip_frame_exact_above_the_psnr_floor() {
    let dir = temp_dir("psnr");
    let path = dir.join("clip.webm");
    let report = encode(&path, &settings(1), None);
    assert_eq!(report.frames, u64::from(FRAMES));
    assert_eq!(report.duration_ns, 2_000_000_000);
    assert!(report.keyframes >= 2, "keyframe interval 24 over 48 frames");
    let bytes = std::fs::read(&path).expect("file");
    assert_eq!(report.bytes, bytes.len() as u64);
    assert!(!dir.join("clip.webm.av1.part").exists() && !dir.join("clip.webm.part").exists());
    let back = read_back(bytes);
    assert_eq!(back.duration_ns, Some(2_000_000_000));
    assert_eq!(back.pictures.len(), FRAMES as usize, "every picture decodes");
    assert_eq!(back.cues as u64, report.cues);
    assert!(back.keyframes[0] && back.cues >= 2);
    let mut worst = f64::INFINITY;
    let mut worst_all = f64::INFINITY;
    for (k, pic) in back.pictures.iter().enumerate() {
        let expect_pts = frame_time_ns(k as u64, FPS, 1) / 1_000_000 * 1_000_000;
        assert_eq!(back.video_pts[k], expect_pts, "packet {k} time");
        assert_eq!(pic.pts_ns, expect_pts, "picture {k} time");
        assert_eq!((pic.width, pic.height, pic.bit_depth), (W, H, 8));
        let src = pattern_frame(W, H, k as u32);
        let y = (W * H) as usize;
        let decoded: Vec<u8> = pic
            .planes
            .iter()
            .flat_map(|plane| plane.iter().copied())
            .collect();
        worst = worst.min(psnr(&src[..y], &pic.planes[0]).expect("luma"));
        worst_all = worst_all.min(psnr(&src, &decoded).expect("all planes"));
    }
    println!(
        "quality {QUALITY} speed 10: worst luma PSNR {worst:.2} dB, worst Y+U+V {worst_all:.2} dB, {} bytes",
        report.bytes
    );
    assert!(
        worst > PSNR_FLOOR_DB,
        "worst luma PSNR {worst:.2} dB at quality {QUALITY}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn higher_quality_gives_higher_psnr_and_a_bigger_file() {
    let dir = temp_dir("ladder");
    let mut last: Option<(f64, u64)> = None;
    for quality in [160u8, 80, 20] {
        let path = dir.join(format!("q{quality}.webm"));
        let report = encode(
            &path,
            &EncodeSettings {
                quality,
                ..settings(1)
            },
            None,
        );
        let back = read_back(std::fs::read(&path).expect("file"));
        let mean = back
            .pictures
            .iter()
            .enumerate()
            .map(|(k, p)| {
                psnr(&pattern_frame(W, H, k as u32)[..(W * H) as usize], &p.planes[0]).expect("luma")
            })
            .sum::<f64>()
            / f64::from(FRAMES);
        println!(
            "quality {quality}: mean luma PSNR {mean:.2} dB, {} bytes",
            report.bytes
        );
        if let Some((psnr_before, bytes_before)) = last {
            assert!(mean > psnr_before && report.bytes > bytes_before);
        }
        last = Some((mean, report.bytes));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sound_round_trips_within_one_opus_packet_and_interleaves_by_time() {
    let dir = temp_dir("audio");
    let path = dir.join("av.webm");
    // 2 s of picture and 2 s of stereo tone at 48 kHz, the render-out mixdown's default shape.
    let pcm = tone(48_000, 2, 96_000);
    let report = encode(&path, &settings(1), Some(&pcm));
    assert_eq!(report.audio_samples, 96_000);
    let back = read_back(std::fs::read(&path).expect("file"));
    assert_eq!(back.audio_channels, 2);
    let decoded = back.audio.len() / 2;
    // The player drops the pre-skip and ignores DiscardPadding: at most one packet (960) longer.
    assert!(
        (96_000..96_000 + 960).contains(&decoded),
        "decoded {decoded} samples per channel for 96000"
    );
    // The tone's level survives (RMS of a 0.5 sine is 0.354), and the start is not shifted by the
    // pre-skip: the first 20 ms correlate with the source.
    let rms = (back.audio.iter().map(|s| s * s).sum::<f32>() / back.audio.len() as f32).sqrt();
    assert!((rms - 0.354).abs() < 0.03, "rms {rms}");
    let corr: f32 = (0..960).map(|i| back.audio[i * 2] * pcm.samples[i * 2]).sum();
    let energy: f32 = (0..960).map(|i| pcm.samples[i * 2] * pcm.samples[i * 2]).sum();
    assert!(corr > 0.8 * energy, "aligned start: {corr} vs {energy}");
    // Interleaving: the file reads in non-decreasing time across both tracks.
    let mut d = MkvDemuxer::open(Cursor::new(std::fs::read(&path).expect("file"))).expect("demux");
    let mut last = i64::MIN;
    while let Some(p) = d.next_packet().expect("packet") {
        assert!(p.pts_ns >= last, "{} after {last}", p.pts_ns);
        last = p.pts_ns;
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn single_threaded_encodes_are_byte_identical() {
    let dir = temp_dir("determinism");
    let pcm = tone(48_000, 1, 48_000);
    let a = dir.join("a.webm");
    let b = dir.join("b.webm");
    encode(&a, &settings(1), Some(&pcm));
    encode(&b, &settings(1), Some(&pcm));
    let (a_bytes, b_bytes) = (std::fs::read(&a).expect("a"), std::fs::read(&b).expect("b"));
    assert_eq!(a_bytes, b_bytes, "threads 1 is deterministic");
    // With four workers the bitstream may differ; the decoded pictures are compared instead.
    let c = dir.join("c.webm");
    encode(&c, &settings(4), Some(&pcm));
    let c_bytes = std::fs::read(&c).expect("c");
    println!("threads 4 byte-identical to threads 1: {}", c_bytes == a_bytes);
    let (pa, pc) = (read_back(a_bytes), read_back(c_bytes));
    assert_eq!(pa.pictures.len(), pc.pictures.len());
    for (x, y) in pa.pictures.iter().zip(&pc.pictures) {
        let p = psnr(&x.planes[0], &y.planes[0]).expect("luma");
        assert!(p > 40.0, "threads 4 picture within 40 dB of threads 1: {p:.2}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_written_file_passes_the_cook_validator() {
    let dir = temp_dir("validate");
    let path = dir.join("movie.webm");
    encode(&path, &settings(1), Some(&tone(48_000, 2, 48_000)));
    let bytes = std::fs::read(&path).expect("file");
    let report = regia_media::validate(Cursor::new(&bytes)).expect("cook validator accepts it");
    assert_eq!((report.width, report.height), (W, H));
    assert_eq!(report.duration_ns, Some(2_000_000_000));
    assert_eq!(report.tracks.len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unfinished_encode_leaves_no_file_and_a_short_one_plays() {
    let dir = temp_dir("cancel");
    let path = dir.join("cut.webm");
    // Dropped without finish (a killed job): nothing at the path, no spool left.
    {
        let mut enc = WebmEncoder::create(&path, W, H, FPS, 1, &settings(1)).expect("encoder");
        for k in 0..10 {
            enc.push_i420(&pattern_frame(W, H, k)).expect("picture");
        }
        assert!(dir.join("cut.webm.av1.part").exists());
    }
    assert!(!path.exists() && !dir.join("cut.webm.av1.part").exists());
    // Finished early (a cancelled render-out): every pushed picture is in a playable file.
    let mut enc = WebmEncoder::create(&path, W, H, FPS, 1, &settings(1)).expect("encoder");
    for k in 0..7 {
        enc.push_i420(&pattern_frame(W, H, k)).expect("picture");
    }
    let report = enc.finish(None).expect("finish");
    assert_eq!(report.frames, 7);
    assert_eq!(report.duration_ns, frame_time_ns(7, FPS, 1) as u64);
    let back = read_back(std::fs::read(&path).expect("file"));
    assert_eq!(back.pictures.len(), 7);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn finish_replaces_an_output_file_held_open_by_a_reader() {
    // Media audit G 2: the "re-render the take and check it in the player" loop. The first render
    // is still open by a reader (a preview, Explorer thumbnailer, browser tab) when the second
    // `finish` lands. Windows refuses a rename over an open file (AccessDenied) and the fix
    // removes the destination and retries once; Linux/macOS replace silently, so this guards the
    // workflow on every OS and only fails pre-fix on Windows.
    let dir = temp_dir("replace");
    let path = dir.join("take.webm");
    encode(&path, &settings(1), None);
    let held = std::fs::File::open(&path).expect("reader holds the first render");
    encode(&path, &settings(1), None);
    drop(held);
    let back = read_back(std::fs::read(&path).expect("the replaced file"));
    assert_eq!(
        back.pictures.len(),
        FRAMES as usize,
        "the new encode is read back"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hostile_inputs_are_refused_with_codes() {
    let dir = temp_dir("hostile");
    let path = dir.join("x.webm");
    let code = |w, h, s: &EncodeSettings| WebmEncoder::create(&path, w, h, FPS, 1, s).err().map(|e| e.code);
    assert_eq!(code(0, 0, &settings(1)), Some("encode_bad_size"));
    assert_eq!(code(127, 71, &settings(1)), Some("encode_odd_size"));
    let huge = EncodeSettings {
        keyframe_interval: 1_000_000,
        ..settings(1)
    };
    assert_eq!(code(W, H, &huge), Some("encode_bad_keyframe_interval"));
    let mut enc = WebmEncoder::create(&path, W, H, FPS, 1, &settings(1)).expect("encoder");
    assert_eq!(
        enc.push_rgba(&[0; 7]).err().map(|e| e.code),
        Some("encode_bad_input")
    );
    enc.push_rgba(&vec![200; (W * H * 4) as usize]).expect("rgba");
    assert_eq!(
        enc.finish(Some(&tone(44_100, 2, 4410))).err().map(|e| e.code),
        Some("encode_audio_format")
    );
    assert!(!path.exists(), "a failed finish leaves no file");
    let _ = std::fs::remove_dir_all(&dir);
}
