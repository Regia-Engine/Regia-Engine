// SPDX-License-Identifier: MPL-2.0
//! `regia media encode <frames dir | file.y4m> [file.wav] -o out.webm` (spec 50 § 14, ADR-3369, book
//! `agents/video-encode.md`): encodes game movies (boot movies, cutscenes, trailers) to AV1 and Opus
//! in WebM with Regia's own encoder, then checks the result with the cook's video validator
//! (`regia_media::validate`, the check `cook` runs on every `.webm`; the cook copies and validates
//! video, it never transcodes).
//!
//! Inputs: a directory of PNG frames (sorted by file name; the `render.out` PNG frames work as they
//! are) at `--fps`, or a YUV4MPEG2 file (8-bit 4:2:0, its own frame rate), plus an optional WAV
//! (PCM 16/24/32 or float 32, mono or stereo, 8/12/16/24/48 kHz) as the sound track. Settings mirror
//! `render.out`'s `encode` block. Without the `media-encode` feature the command answers
//! `media_encode_unavailable`.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use clap::{Args, Subcommand};
use regia_media::encode::{EncodeError, EncodeReport, EncodeSettings};

/// `regia media` subcommands.
#[derive(Debug, Subcommand)]
pub enum MediaCommand {
    /// Encode a frames directory or `.y4m` (plus an optional `.wav`) to AV1 and Opus in WebM, then
    /// validate it as the cook would.
    Encode(EncodeArgs),
}

/// `regia media encode`.
#[derive(Debug, Args)]
pub struct EncodeArgs {
    /// The pictures (a directory of PNG frames, or a `.y4m` file), then optionally a `.wav`.
    #[arg(required = true, num_args = 1..=2)]
    pub inputs: Vec<PathBuf>,
    /// The `.webm` to write.
    #[arg(short, long)]
    pub out: PathBuf,
    /// Frame rate of a frames directory (a `.y4m` carries its own).
    #[arg(long, default_value_t = 30)]
    pub fps: u32,
    /// Frame-rate denominator (1001 for 29.97).
    #[arg(long, default_value_t = 1)]
    pub fps_den: u32,
    /// Constant quality: the AV1 quantizer 1..=255, lower is better.
    #[arg(long)]
    pub quality: Option<u8>,
    /// Target bitrate in kbit/s instead of constant quality.
    #[arg(long)]
    pub bitrate_kbps: Option<u32>,
    /// Speed preset 0 (slowest, smallest) ..= 10 (fastest).
    #[arg(long)]
    pub speed: Option<u8>,
    /// Longest run of frames between keyframes, 1..=600.
    #[arg(long)]
    pub keyframe_interval: Option<u32>,
    /// Opus bitrate in kbit/s, 16..=510.
    #[arg(long)]
    pub audio_kbps: Option<u32>,
    /// Encoder threads (0: half the cores; 1: byte-identical output run to run).
    #[arg(long)]
    pub threads: Option<u32>,
    /// Print the report as JSON.
    #[arg(long)]
    pub json: bool,
}

/// Runs a `regia media` subcommand.
pub fn run(command: &MediaCommand) -> Result<()> {
    match command {
        MediaCommand::Encode(args) => {
            check_out(args)?;
            let report = encode(args).map_err(|e| anyhow!("{}: {}", e.code, e.message))?;
            let clip = validate(&args.out)?;
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "encode": report, "validate": clip }))?
                );
            } else {
                println!(
                    "wrote {}: {} frames ({} keyframes), {:.3} s, {} bytes, {} Opus packets; cook validator: ok ({} tracks, {} cues)",
                    args.out.display(),
                    report.frames,
                    report.keyframes,
                    report.duration_ns as f64 / 1e9,
                    report.bytes,
                    report.audio_packets,
                    clip.tracks.len(),
                    clip.cue_count
                );
            }
            Ok(())
        }
    }
}

/// The settings of `args` over the defaults, checked.
pub fn settings(args: &EncodeArgs) -> Result<EncodeSettings, EncodeError> {
    let d = EncodeSettings::default();
    let s = EncodeSettings {
        quality: args.quality.unwrap_or(d.quality),
        bitrate_kbps: args.bitrate_kbps.unwrap_or(d.bitrate_kbps),
        speed: args.speed.unwrap_or(d.speed),
        keyframe_interval: args.keyframe_interval.unwrap_or(d.keyframe_interval),
        audio_kbps: args.audio_kbps.unwrap_or(d.audio_kbps),
        threads: args.threads.unwrap_or(d.threads),
    };
    s.check()?;
    Ok(s)
}

/// The cook's video validator on the written file (`regia_media::validate`).
fn validate(path: &Path) -> Result<regia_media::validate::ClipReport> {
    let file = std::fs::File::open(path).map_err(|e| anyhow!("encode_io: {}: {e}", path.display()))?;
    regia_media::validate(std::io::BufReader::new(file)).map_err(|e| {
        anyhow!(
            "the cook's validator refused {}: {} ({e})",
            path.display(),
            e.code()
        )
    })
}

fn bad(message: impl Into<String>) -> EncodeError {
    EncodeError::new("encode_bad_input", message)
}

/// Splits the inputs into the pictures and the optional WAV.
fn inputs(args: &EncodeArgs) -> Result<(PathBuf, Option<PathBuf>), EncodeError> {
    let is_wav = |p: &Path| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("wav"));
    let (wavs, pictures): (Vec<&PathBuf>, Vec<&PathBuf>) = args.inputs.iter().partition(|p| is_wav(p));
    match (pictures.as_slice(), wavs.as_slice()) {
        ([pictures], []) => Ok(((*pictures).clone(), None)),
        ([pictures], [wav]) => Ok(((*pictures).clone(), Some((*wav).clone()))),
        ([], _) => Err(bad(
            "a WebM movie needs pictures: give a frames directory or a .y4m file (and the .wav after it)",
        )),
        _ => Err(bad("give one picture source and at most one .wav")),
    }
}

/// Encodes `args` and returns the encoder's report (`media_encode_unavailable` without the feature).
pub fn encode(args: &EncodeArgs) -> Result<EncodeReport, EncodeError> {
    let settings = settings(args)?;
    let (pictures, wav) = inputs(args)?;
    if !regia_media::encode::available() {
        return Err(EncodeError::unavailable());
    }
    let audio = match &wav {
        Some(path) => {
            let bytes = std::fs::read(path).map_err(|e| EncodeError::io(path, &e))?;
            Some(regia_media::encode::wav::parse(&bytes)?)
        }
        None => None,
    };
    if let Some(parent) = args.out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| EncodeError::io(parent, &e))?;
    }
    imp::encode(&pictures, args, &settings, audio.as_ref())
}

#[cfg(feature = "media-encode")]
/// The PNG files of a frames directory, sorted by name.
pub(super) fn frame_files(dir: &Path) -> Result<Vec<PathBuf>, EncodeError> {
    let entries = std::fs::read_dir(dir).map_err(|e| EncodeError::io(dir, &e))?;
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")))
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(bad(format!("{}: no .png frames", dir.display())));
    }
    Ok(files)
}

#[cfg(feature = "media-encode")]
mod imp {
    use std::path::Path;

    use regia_media::encode::wav::Pcm;
    use regia_media::encode::webm::WebmEncoder;
    use regia_media::encode::y4m::Y4mReader;
    use regia_media::encode::{EncodeError, EncodeReport, EncodeSettings};

    use super::{EncodeArgs, bad, frame_files};

    pub(super) fn encode(
        pictures: &Path,
        args: &EncodeArgs,
        settings: &EncodeSettings,
        audio: Option<&Pcm>,
    ) -> Result<EncodeReport, EncodeError> {
        if pictures.is_dir() {
            let files = frame_files(pictures)?;
            let mut encoder: Option<WebmEncoder> = None;
            let mut size = (0, 0);
            for file in &files {
                let image = image::open(file)
                    .map_err(|e| bad(format!("{}: {e}", file.display())))?
                    .to_rgba8();
                let (w, h) = image.dimensions();
                let enc = match &mut encoder {
                    Some(enc) => {
                        if (w, h) != size {
                            return Err(bad(format!(
                                "{} is {w}x{h}; the first frame was {}x{}",
                                file.display(),
                                size.0,
                                size.1
                            )));
                        }
                        enc
                    }
                    None => {
                        size = (w, h);
                        encoder.insert(WebmEncoder::create(
                            &args.out,
                            w,
                            h,
                            args.fps,
                            args.fps_den,
                            settings,
                        )?)
                    }
                };
                enc.push_rgba(image.as_raw())?;
            }
            encoder.ok_or_else(|| bad("no frames"))?.finish(audio)
        } else {
            let file = std::fs::File::open(pictures).map_err(|e| EncodeError::io(pictures, &e))?;
            let mut reader = Y4mReader::new(std::io::BufReader::with_capacity(1 << 20, file))?;
            let h = reader.header();
            let mut encoder =
                WebmEncoder::create(&args.out, h.width, h.height, h.fps_num, h.fps_den, settings)?;
            while let Some(frame) = reader.next_frame()? {
                encoder.push_i420(&frame)?;
            }
            if encoder.frames() == 0 {
                return Err(bad(format!("{}: no frames", pictures.display())));
            }
            encoder.finish(audio)
        }
    }
}

#[cfg(not(feature = "media-encode"))]
mod imp {
    use std::path::Path;

    use regia_media::encode::wav::Pcm;
    use regia_media::encode::{EncodeError, EncodeReport, EncodeSettings};

    use super::EncodeArgs;

    pub(super) fn encode(
        _pictures: &Path,
        _args: &EncodeArgs,
        _settings: &EncodeSettings,
        _audio: Option<&Pcm>,
    ) -> Result<EncodeReport, EncodeError> {
        Err(EncodeError::unavailable())
    }
}

/// `regia media encode` is a file tool: refuse to overwrite one of its own inputs.
pub fn check_out(args: &EncodeArgs) -> Result<()> {
    if args.inputs.iter().any(|i| i == &args.out) {
        bail!(
            "encode_bad_input: the output {} is also an input",
            args.out.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(inputs: &[&str], out: &Path) -> EncodeArgs {
        EncodeArgs {
            inputs: inputs.iter().map(PathBuf::from).collect(),
            out: out.to_path_buf(),
            fps: 24,
            fps_den: 1,
            quality: None,
            bitrate_kbps: None,
            speed: Some(10),
            keyframe_interval: Some(12),
            audio_kbps: None,
            threads: Some(1),
            json: false,
        }
    }

    #[test]
    fn hostile_settings_and_inputs_are_refused_with_codes() {
        let dir = regia_test_utils::unique_temp_dir("media-encode-hostile");
        let out = dir.join("x.webm");
        let mut a = args(&["frames"], &out);
        a.keyframe_interval = Some(1_000_000);
        assert_eq!(
            encode(&a).err().map(|e| e.code),
            Some("encode_bad_keyframe_interval")
        );
        let mut a = args(&["frames"], &out);
        a.quality = Some(0);
        assert_eq!(encode(&a).err().map(|e| e.code), Some("encode_bad_quality"));
        let a = args(&["sound.wav"], &out);
        assert_eq!(encode(&a).err().map(|e| e.code), Some("encode_bad_input"));
        let a = args(&["a.y4m", "b.y4m"], &out);
        assert_eq!(encode(&a).err().map(|e| e.code), Some("encode_bad_input"));
        let a = args(&[out.to_str().expect("utf8")], &out);
        assert!(check_out(&a).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(not(feature = "media-encode"))]
    #[test]
    fn without_the_feature_the_command_reports_unavailable() {
        let dir = regia_test_utils::unique_temp_dir("media-encode-off");
        let a = args(&["frames"], &dir.join("x.webm"));
        assert_eq!(encode(&a).err().map(|e| e.code), Some("media_encode_unavailable"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "media-encode")]
    #[test]
    fn a_frames_directory_and_a_y4m_encode_and_pass_the_cook_validator() {
        let dir = regia_test_utils::unique_temp_dir("media-encode-cli");
        // 12 PNG frames of the pattern clip, converted to RGBA, plus a 0.5 s stereo WAV.
        let frames = dir.join("frames");
        std::fs::create_dir_all(&frames).expect("frames dir");
        for k in 0..12u32 {
            let img = image::RgbaImage::from_fn(64, 48, |x, y| {
                image::Rgba([(x * 4 + k * 9) as u8, (y * 5) as u8, ((x + y + k) * 3) as u8, 255])
            });
            img.save(frames.join(format!("shot_{k:06}.png"))).expect("png");
        }
        let tone: Vec<f32> = (0..24_000 * 2)
            .map(|i| ((i / 2) as f32 * 0.06).sin() * 0.4)
            .collect();
        let wav = dir.join("mix.wav");
        std::fs::write(&wav, regia_media::encode::wav::write_pcm16(48_000, 2, &tone)).expect("wav");
        let out = dir.join("out/movie.webm");
        let a = args(
            &[frames.to_str().expect("utf8"), wav.to_str().expect("utf8")],
            &out,
        );
        let report = encode(&a).expect("encode");
        assert_eq!((report.frames, report.duration_ns), (12, 500_000_000));
        assert!(report.audio_packets > 0);
        let clip = validate(&out).expect("cook validator");
        assert_eq!((clip.width, clip.height, clip.tracks.len()), (64, 48, 2));
        // A .y4m with its own rate.
        let mut y4m = b"YUV4MPEG2 W32 H32 F25:1 Ip C420jpeg\n".to_vec();
        for k in 0..5u8 {
            y4m.extend_from_slice(b"FRAME\n");
            y4m.extend(std::iter::repeat_n(k * 40, 32 * 32 * 3 / 2));
        }
        let src = dir.join("clip.y4m");
        std::fs::write(&src, y4m).expect("y4m");
        let out = dir.join("clip.webm");
        let report = encode(&args(&[src.to_str().expect("utf8")], &out)).expect("encode y4m");
        assert_eq!((report.frames, report.duration_ns), (5, 200_000_000));
        assert!(validate(&out).is_ok());
        // Frames of different sizes are refused, and nothing is left behind.
        image::RgbaImage::new(32, 32)
            .save(frames.join("shot_999999.png"))
            .expect("png");
        let out = dir.join("bad.webm");
        assert_eq!(
            encode(&args(&[frames.to_str().expect("utf8")], &out))
                .err()
                .map(|e| e.code),
            Some("encode_bad_input")
        );
        assert!(!out.exists() && !dir.join("bad.webm.av1.part").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
