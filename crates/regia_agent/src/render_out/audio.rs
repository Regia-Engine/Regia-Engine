// SPDX-License-Identifier: MPL-2.0
//! Audio in render-out (`render.out_start` `audio`, ADR-1520, ADR-1554): a WAV mixdown of the
//! master output and optional per-bus stems, beside the frames.
//!
//! The job runs the mixer offline (`regia_audio::Audio::begin_mixdown`): a fresh kira mixer at the
//! requested rate that advances only when the job renders a block. After every delivered frame the
//! job renders that frame's block, so a sound started on frame `i` starts at frame `i`'s sample.
//! The block sizes come from integers alone. `o(i) = ⌊i · rate · fps_den / fps⌋` is the sample
//! output frame `i` starts at, `g(t) = ⌊t · rate / 10⁹⌋` the samples in `t` ns, and `G(i)` the
//! simulation time of frame `i` (the time map's sum, ADR-1524). Which clock the mixer follows is
//! `timeline`:
//!
//! - `render` (the default): the render clock, the clock VFX, shots and HUD animation run on.
//!   Frame `i` takes and writes `o(i+1) − o(i)` samples, so the WAV is exactly as long as the video
//!   and a time map changes neither the speed nor the pitch of a sound (slow-motion picture,
//!   real-time sound effects);
//! - `output`: the simulation clock, resampled to the video. The mixer advances `g(G(i+1)) −
//!   g(G(i))` samples and the block is resampled to `o(i+1) − o(i)` (varispeed, like tape: slow
//!   motion is lower and longer, a speed-up higher and shorter). A slowed block is interpolated
//!   linearly from the last sample of the block before; a sped-up one is box-filtered (each output
//!   sample averages the input it covers, with exact integer weights);
//! - `sim`: the simulation clock, unresampled. The WAV follows `sim_time_ns` (frame `i` starts at
//!   `g(G(i))`), for an editor that conforms the sound itself.
//!
//! The length is exact: a job of `N` frames writes `⌊N · rate · fps_den / fps⌋` samples, so a whole
//! number of seconds is `seconds · rate` (ADR-1656). The one exception is `sim` under a real time
//! map, whose length is `⌊sim_time_ns · rate / 10⁹⌋` of the nanosecond simulation clock and can be
//! one sample short of the ideal real-valued duration.
//!
//! Without a time map the three are the same file: the frame step is the rational step, not its
//! nanosecond rounding, so nothing is resampled.
//!
//! Warm-up frames (and the frame `render.out_start` ran in) render one pre-roll block each, of the
//! warm-up step on the timeline's clock, and throw it away, so the mixer's clock is the job's at
//! frame 0.
//!
//! Samples are written as they are made, to `<file>.part`; the header is written and the file
//! renamed when the job ends (done, cancelled or failed). The WAV then covers exactly the frames
//! kept on disk. The hash of each file is FNV-1a over its data chunk (the sample bytes).
//!
//! Closing a WAV syncs it before the rename, so a file under its final name is whole on disk. That
//! sync can take a minute on a busy disk, so it never runs on the app thread: the job ends the
//! mixdown there ([`AudioOut::end`]) and closes the files on its closing thread
//! ([`AudioClose::close`], ADR-1881).

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_app::prelude::*;
use regia_audio::{Audio, MAX_MIXDOWN_STEMS, MIXDOWN_SAMPLE_RATES, MixdownSettings};
use serde_json::{Value, json};

use super::plan::Request;
use crate::protocol::{self, RpcError};

/// The default mixdown sample rate, Hz.
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;

/// Largest WAV the job writes: the RIFF size field is 32 bits.
const MAX_WAV_BYTES: u64 = u32::MAX as u64;

/// The keys an `audio` object may carry.
const AUDIO_KEYS: [&str; 4] = ["sample_rate", "format", "stems", "timeline"];

/// How each sample is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    /// 16-bit signed integer PCM.
    Pcm16,
    /// 24-bit signed integer PCM (the default).
    Pcm24,
    /// 32-bit IEEE float (the mixer's own samples, unquantized).
    Float32,
}

impl SampleFormat {
    /// Every sample format, in the order the protocol lists them.
    pub const ALL: [Self; 3] = [Self::Pcm16, Self::Pcm24, Self::Float32];

    /// Parses the `audio.format` parameter.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "pcm16" => Some(Self::Pcm16),
            "pcm24" => Some(Self::Pcm24),
            "float32" => Some(Self::Float32),
            _ => None,
        }
    }

    /// Stable name (protocol, manifest).
    pub fn name(self) -> &'static str {
        match self {
            Self::Pcm16 => "pcm16",
            Self::Pcm24 => "pcm24",
            Self::Float32 => "float32",
        }
    }

    /// Bits per sample.
    pub fn bits(self) -> u16 {
        match self {
            Self::Pcm16 => 16,
            Self::Pcm24 => 24,
            Self::Float32 => 32,
        }
    }

    /// Bytes per sample.
    fn bytes(self) -> u64 {
        u64::from(self.bits() / 8)
    }

    /// The WAV format tag: 1 = integer PCM, 3 = IEEE float.
    fn tag(self) -> u16 {
        if self == Self::Float32 { 3 } else { 1 }
    }

    /// Bytes before the data: RIFF, `fmt ` (18 bytes for float, with `cbSize`), a `fact` chunk for
    /// float, and the data chunk header.
    fn header_len(self) -> u64 {
        if self == Self::Float32 { 58 } else { 44 }
    }

    /// Appends `sample` (clamped to ±1, NaN as 0) in this format, little-endian.
    fn push(self, sample: f32, out: &mut Vec<u8>) {
        let x = if sample.is_nan() {
            0.0
        } else {
            sample.clamp(-1.0, 1.0)
        };
        match self {
            Self::Pcm16 => out.extend_from_slice(&((x * 32_767.0).round() as i16).to_le_bytes()),
            Self::Pcm24 => {
                let code = (x * 8_388_607.0).round() as i32;
                out.extend_from_slice(&code.to_le_bytes()[..3]);
            }
            Self::Float32 => out.extend_from_slice(&x.to_le_bytes()),
        }
    }
}

/// Which clock the WAV's samples follow (ADR-1554).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timeline {
    /// The render clock's (the default): the mixer steps by the output frame step, so a time map
    /// changes neither the WAV's length nor its pitch.
    Render,
    /// The simulation clock's, resampled to the video's length: a time map is varispeed.
    Output,
    /// The simulation clock's, unresampled: a time map changes the WAV's length, not its pitch.
    Sim,
}

impl Timeline {
    /// Every timeline, in the order the protocol lists them.
    pub const ALL: [Self; 3] = [Self::Render, Self::Output, Self::Sim];

    /// Parses the `audio.timeline` parameter.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "render" => Some(Self::Render),
            "output" => Some(Self::Output),
            "sim" => Some(Self::Sim),
            _ => None,
        }
    }

    /// Stable name (protocol, manifest).
    pub fn name(self) -> &'static str {
        match self {
            Self::Render => "render",
            Self::Output => "output",
            Self::Sim => "sim",
        }
    }

    /// The clock the mixer steps by: `"render"` or `"sim"` (the manifest's `clock`).
    pub fn clock(self) -> &'static str {
        match self {
            Self::Render => "render",
            Self::Output | Self::Sim => "sim",
        }
    }

    /// True when the mixer steps by the simulation clock.
    fn follows_sim(self) -> bool {
        self != Self::Render
    }
}

/// A validated `render.out_start` `audio` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioRequest {
    /// Mixdown sample rate, Hz.
    pub sample_rate: u32,
    /// Sample format of every file.
    pub format: SampleFormat,
    /// Buses recorded to their own WAV, in request order.
    pub stems: Vec<String>,
    /// The clock the WAV follows.
    pub timeline: Timeline,
}

impl Default for AudioRequest {
    fn default() -> Self {
        Self {
            sample_rate: DEFAULT_SAMPLE_RATE,
            format: SampleFormat::Pcm24,
            stems: Vec::new(),
            timeline: Timeline::Render,
        }
    }
}

/// A bus name usable in a file name: 1..=64 letters, digits, `_`, `-` or `.`, not leading `.`.
fn file_safe(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

impl AudioRequest {
    /// Parses `audio`: absent, `null` or `false` is no audio; `true` is the defaults; an object
    /// sets `sample_rate`, `format`, `stems` and `timeline` (other keys are refused).
    pub fn parse(value: Option<&Value>) -> Result<Option<Self>, String> {
        let object = match value {
            None | Some(Value::Null) | Some(Value::Bool(false)) => return Ok(None),
            Some(Value::Bool(true)) => return Ok(Some(Self::default())),
            Some(Value::Object(object)) => object,
            Some(_) => {
                return Err(
                    "`audio` must be true, false or an object { sample_rate?, format?, stems?, timeline? }"
                        .into(),
                );
            }
        };
        if let Some(key) = object.keys().find(|k| !AUDIO_KEYS.contains(&k.as_str())) {
            return Err(format!(
                "unknown `audio` key `{key}` (sample_rate, format, stems, timeline)"
            ));
        }
        let mut request = Self::default();
        if let Some(value) = object.get("sample_rate").filter(|v| !v.is_null()) {
            request.sample_rate = value
                .as_u64()
                .and_then(|rate| u32::try_from(rate).ok())
                .filter(|rate| MIXDOWN_SAMPLE_RATES.contains(rate))
                .ok_or_else(|| {
                    format!(
                        "`audio.sample_rate` must be an integer in {}..={} (Hz)",
                        MIXDOWN_SAMPLE_RATES.start(),
                        MIXDOWN_SAMPLE_RATES.end()
                    )
                })?;
        }
        if let Some(value) = object.get("format").filter(|v| !v.is_null()) {
            request.format = value.as_str().and_then(SampleFormat::parse).ok_or_else(|| {
                "`audio.format` must be \"pcm16\", \"pcm24\" (default) or \"float32\"".to_string()
            })?;
        }
        if let Some(value) = object.get("timeline").filter(|v| !v.is_null()) {
            request.timeline = value.as_str().and_then(Timeline::parse).ok_or_else(|| {
                "`audio.timeline` must be \"render\" (default), \"output\" or \"sim\"".to_string()
            })?;
        }
        match object.get("stems") {
            None | Some(Value::Null) => {}
            Some(Value::Array(list)) => {
                if list.len() > MAX_MIXDOWN_STEMS {
                    return Err(format!(
                        "`audio.stems` has {} entries; the limit is {MAX_MIXDOWN_STEMS}",
                        list.len()
                    ));
                }
                for entry in list {
                    let name = entry
                        .as_str()
                        .filter(|name| file_safe(name))
                        .ok_or_else(|| {
                            "`audio.stems` entries must be bus names of 1..=64 letters, digits, `_`, `-` or `.` (not leading `.`)"
                                .to_string()
                        })?;
                    if request.stems.iter().any(|s| s == name) {
                        return Err(format!("`audio.stems` lists `{name}` twice"));
                    }
                    request.stems.push(name.to_string());
                }
            }
            Some(_) => return Err("`audio.stems` must be an array of bus names".into()),
        }
        Ok(Some(request))
    }

    /// The request as the echo and the manifest show it.
    pub fn to_json(&self) -> Value {
        json!({
            "sample_rate": self.sample_rate,
            "format": self.format.name(),
            "bits": self.format.bits(),
            "channels": 2,
            "stems": self.stems,
            "timeline": self.timeline.name(),
            "clock": self.timeline.clock(),
        })
    }

    /// What the mixer is asked to record.
    pub fn mixdown(&self) -> MixdownSettings {
        MixdownSettings {
            sample_rate: self.sample_rate,
            stems: self.stems.clone(),
        }
    }
}

/// Samples before output frame `index`: `⌊index · rate · fps_den / fps⌋`.
pub fn output_offset(index: u64, rate: u32, fps_num: u32, fps_den: u32) -> u64 {
    let n = u128::from(index) * u128::from(rate) * u128::from(fps_den);
    (n / u128::from(fps_num.max(1))) as u64
}

/// Samples in `nanos` nanoseconds of a clock: `⌊nanos · rate / 10⁹⌋`.
pub fn clock_offset(nanos: u64, rate: u32) -> u64 {
    (u128::from(nanos) * u128::from(rate) / 1_000_000_000) as u64
}

/// Simulation time of delivered frame `index` after frame 0, nanoseconds (the time map's sum).
fn sim_ns_at(request: &Request, index: u64) -> u64 {
    request.time_map.sim_ns(request.step_ns(), index)
}

/// True when the WAV follows the simulation clock's length (`timeline: "sim"` under a map).
fn sim_length(request: &Request, audio: &AudioRequest) -> bool {
    audio.timeline == Timeline::Sim && !request.time_map.is_identity()
}

/// The sample frame delivered frame `index` starts at in the WAV (`sim_ns` = its simulation
/// time).
pub(crate) fn file_offset(request: &Request, audio: &AudioRequest, index: u64, sim_ns: u64) -> u64 {
    if sim_length(request, audio) {
        clock_offset(sim_ns, audio.sample_rate)
    } else {
        output_offset(index, audio.sample_rate, request.fps_num, request.fps_den)
    }
}

/// How many sample frames delivered frame `index` (starting at simulation time `sim_ns`) takes
/// from the mixer and how many it writes.
pub fn frame_samples(request: &Request, audio: &AudioRequest, index: u64, sim_ns: u64) -> (u64, u64) {
    let rate = audio.sample_rate;
    let span = output_offset(index + 1, rate, request.fps_num, request.fps_den)
        - output_offset(index, rate, request.fps_num, request.fps_den);
    let mix = if !audio.timeline.follows_sim() || request.time_map.is_identity() {
        span
    } else {
        let end = sim_ns.saturating_add(request.sim_step_ns(index));
        clock_offset(end, rate) - clock_offset(sim_ns, rate)
    };
    let out = match audio.timeline {
        Timeline::Render | Timeline::Output => span,
        Timeline::Sim => mix,
    };
    (mix, out)
}

/// Sample frames the job's WAVs will hold when every frame is delivered.
pub fn total_samples(request: &Request, audio: &AudioRequest) -> u64 {
    let end_ns = if sim_length(request, audio) {
        sim_ns_at(request, request.frames)
    } else {
        0
    };
    file_offset(request, audio, request.frames, end_ns)
}

/// The shown paths of the WAVs in a manifest `audio` block (the mix, then each stem), for the
/// manifest's `files.audio` list.
pub fn block_files(block: &Value) -> Vec<Value> {
    let stems = block["stems"].as_array().into_iter().flatten();
    std::iter::once(&block["mix"])
        .chain(stems)
        .filter_map(|file| file.get("file").filter(|f| f.is_string()).cloned())
        .collect()
}

/// Refuses an audio request whose WAV would pass the 4 GiB RIFF limit.
pub fn check_wav_size(request: &Request, audio: &AudioRequest) -> Result<(), String> {
    let bytes = total_samples(request, audio)
        .saturating_mul(2 * audio.format.bytes())
        .saturating_add(audio.format.header_len());
    if bytes > MAX_WAV_BYTES {
        return Err(format!(
            "the audio would be {bytes} bytes per WAV, over the 4 GiB WAV limit; use a lower `audio.sample_rate`, `audio.format` \"pcm16\", or fewer frames"
        ));
    }
    Ok(())
}

/// Resamples a block of interleaved stereo to a given length (varispeed), keeping the last input
/// frame for the next block.
#[derive(Debug, Clone, Copy, Default)]
pub struct Varispeed {
    last: [f32; 2],
}

impl Varispeed {
    /// `input` (interleaved stereo, `input.len() / 2` frames) as exactly `frames` frames.
    pub fn process(&mut self, input: &[f32], frames: usize) -> Vec<f32> {
        let n_in = input.len() / 2;
        let at = |i: usize, ch: usize| input[2 * i + ch];
        let mut out = Vec::with_capacity(frames * 2);
        if n_in == 0 {
            // Frozen simulation time: the mixer did not move, so the frame is silent. This also
            // covers an empty block for an empty frame (`frames == 0`), which has no last frame.
            out.resize(frames * 2, 0.0);
            self.last = [0.0; 2];
            return out;
        } else if n_in == frames {
            out.extend_from_slice(&input[..2 * n_in]);
        } else if n_in > frames {
            // Box filter: output j averages input over [j·n_in, (j+1)·n_in) in units of
            // 1/frames of an input frame; input i covers [i·frames, (i+1)·frames).
            let (n, m) = (n_in as u64, frames as u64);
            for j in 0..m {
                let (lo, hi) = (j * n, (j + 1) * n);
                let mut sum = [0.0_f64; 2];
                for i in lo / m..hi.div_ceil(m).min(n) {
                    let overlap = hi.min((i + 1) * m) - lo.max(i * m);
                    for (ch, acc) in sum.iter_mut().enumerate() {
                        *acc += overlap as f64 * f64::from(at(i as usize, ch));
                    }
                }
                for acc in sum {
                    out.push((acc / n as f64) as f32);
                }
            }
        } else {
            // Linear: output j sits at input position (j+1)·n_in/frames − 1, with input −1 being
            // the last frame of the block before.
            let (n, m) = (n_in as i64, frames as i64);
            let sample = |i: i64, ch: usize| {
                if i < 0 { self.last[ch] } else { at(i as usize, ch) }
            };
            for j in 0..m {
                let position = (j + 1) * n - m;
                let i = position.div_euclid(m);
                let frac = position.rem_euclid(m) as f64 / m as f64;
                for ch in 0..2 {
                    let a = f64::from(sample(i, ch));
                    let b = if i + 1 < n {
                        f64::from(sample(i + 1, ch))
                    } else {
                        a
                    };
                    out.push((a + (b - a) * frac) as f32);
                }
            }
        }
        self.last = [at(n_in - 1, 0), at(n_in - 1, 1)];
        out
    }
}

/// Folds `bytes` into an FNV-1a 64 hash (`super::plan::fnv1a` is the same from its start value).
fn fnv1a_extend(mut hash: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The FNV-1a 64 start value.
const FNV_START: u64 = 0xcbf2_9ce4_8422_2325;

/// A function that makes an open file durable ([`RenderOutFileSync`]).
pub type SyncFile = dyn Fn(&File) -> std::io::Result<()> + Send + Sync;

/// What makes a finished render-out file durable before its rename: [`File::sync_all`] unless
/// replaced. Runs on the job's closing thread, never on the app thread (ADR-1881). A host with its
/// own storage rules, or a test standing in for a slow disk, inserts its own.
#[derive(Resource, Clone)]
pub struct RenderOutFileSync(pub Arc<SyncFile>);

impl Default for RenderOutFileSync {
    fn default() -> Self {
        Self(Arc::new(File::sync_all))
    }
}

/// One WAV file being written: samples go to `<file>.part`; the header is written at the end.
struct WavWriter {
    part: PathBuf,
    path: PathBuf,
    file: BufWriter<File>,
    format: SampleFormat,
    rate: u32,
    /// Sample frames written.
    frames: u64,
    hash: u64,
    scratch: Vec<u8>,
}

/// A finished WAV.
struct WavFile {
    path: PathBuf,
    frames: u64,
    hash: u64,
}

fn part_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    path.with_file_name(name)
}

/// The WAV header for `frames` stereo sample frames.
fn wav_header(format: SampleFormat, rate: u32, frames: u64) -> Vec<u8> {
    let channels = 2_u16;
    let block = channels * format.bits() / 8;
    let data = (frames * u64::from(block)) as u32;
    let float = format == SampleFormat::Float32;
    let fmt_len: u32 = if float { 18 } else { 16 };
    let riff = (format.header_len() - 8) as u32 + data;
    let mut h = Vec::with_capacity(format.header_len() as usize);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&riff.to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&fmt_len.to_le_bytes());
    h.extend_from_slice(&format.tag().to_le_bytes());
    h.extend_from_slice(&channels.to_le_bytes());
    h.extend_from_slice(&rate.to_le_bytes());
    h.extend_from_slice(&(rate * u32::from(block)).to_le_bytes());
    h.extend_from_slice(&block.to_le_bytes());
    h.extend_from_slice(&format.bits().to_le_bytes());
    if float {
        h.extend_from_slice(&0_u16.to_le_bytes());
        h.extend_from_slice(b"fact");
        h.extend_from_slice(&4_u32.to_le_bytes());
        h.extend_from_slice(&(frames as u32).to_le_bytes());
    }
    h.extend_from_slice(b"data");
    h.extend_from_slice(&data.to_le_bytes());
    h
}

impl WavWriter {
    fn create(path: PathBuf, format: SampleFormat, rate: u32) -> Result<Self, String> {
        let part = part_path(&path);
        // Read access too: a cut re-hashes the kept samples from the file.
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&part)
            .map_err(|e| format!("{}: {e}", part.display()))?;
        let mut file = BufWriter::with_capacity(1 << 16, file);
        // A placeholder header, rewritten with the sizes at the end.
        file.write_all(&wav_header(format, rate, 0))
            .map_err(|e| format!("{}: {e}", part.display()))?;
        Ok(Self {
            part,
            path,
            file,
            format,
            rate,
            frames: 0,
            hash: FNV_START,
            scratch: Vec::new(),
        })
    }

    /// Appends interleaved stereo samples.
    fn write(&mut self, samples: &[f32]) -> Result<(), String> {
        self.scratch.clear();
        for &sample in samples {
            self.format.push(sample, &mut self.scratch);
        }
        self.file
            .write_all(&self.scratch)
            .map_err(|e| format!("{}: {e}", self.part.display()))?;
        self.hash = fnv1a_extend(self.hash, &self.scratch);
        self.frames += (samples.len() / 2) as u64;
        Ok(())
    }

    /// Cuts the file to `keep` sample frames (when fewer), writes the header, syncs it and renames
    /// it into place. Blocks on the disk: closing thread only.
    fn finish(self, keep: u64, sync: &RenderOutFileSync) -> Result<WavFile, String> {
        let shown = |e: std::io::Error| format!("{}: {e}", self.part.display());
        let mut file = self.file.into_inner().map_err(|e| shown(e.into_error()))?;
        let block = 2 * self.format.bytes();
        let header = self.format.header_len();
        let mut frames = self.frames;
        let mut hash = self.hash;
        if keep < frames {
            frames = keep;
            file.set_len(header + keep * block).map_err(shown)?;
            // Re-hash the kept data.
            file.seek(SeekFrom::Start(header)).map_err(shown)?;
            let mut kept = Vec::new();
            (&mut file)
                .take(keep * block)
                .read_to_end(&mut kept)
                .map_err(shown)?;
            hash = fnv1a_extend(FNV_START, &kept);
        }
        file.seek(SeekFrom::Start(0)).map_err(shown)?;
        file.write_all(&wav_header(self.format, self.rate, frames))
            .map_err(shown)?;
        sync.0(&file).map_err(shown)?;
        drop(file);
        std::fs::rename(&self.part, &self.path).map_err(shown)?;
        Ok(WavFile {
            path: self.path,
            frames,
            hash,
        })
    }

    /// Removes the unfinished file (a job that never started).
    fn discard(self) {
        drop(self.file);
        let _ = std::fs::remove_file(&self.part);
    }
}

/// Makes the renames in `dir` durable too: POSIX keeps a rename in the directory, not in the file.
/// Windows cannot open a directory this way and some filesystems refuse; the files are synced
/// already, so neither fails the job.
fn sync_dir(dir: &Path, sync: &RenderOutFileSync) {
    if cfg!(unix)
        && let Ok(handle) = File::open(dir)
        && let Err(error) = sync.0(&handle)
    {
        tracing::debug!(target: "regia_agent", "render-out directory sync: {error}");
    }
}

/// The WAV of the mix: `<name>_audio.wav`.
pub(super) fn mix_path(request: &Request) -> PathBuf {
    request.out_dir.join(format!("{}_audio.wav", request.name))
}

/// The WAV of a stem: `<name>_audio_<bus>.wav`.
fn stem_path(request: &Request, bus: &str) -> PathBuf {
    request.out_dir.join(format!("{}_audio_{bus}.wav", request.name))
}

/// Checks an audio request against the app before anything is created: the app has an `Audio`
/// resource (`F0013` otherwise) and every stem is one of its buses.
pub fn check(world: &World, audio: &AudioRequest) -> Result<(), RpcError> {
    let Some(mixer) = world.get_resource::<Audio>() else {
        return Err(RpcError::new(
            protocol::UNSUPPORTED,
            "F0013",
            "`audio` needs the mixer and the app has no Audio resource",
            "add regia_audio::AudioPlugin, or leave `audio` out",
        ));
    };
    mixer
        .check_mixdown(&audio.mixdown())
        .map_err(|e| RpcError::invalid_params(format!("`audio`: {e}")))
}

/// The audio side of a running job.
pub struct AudioOut {
    request: AudioRequest,
    mix: WavWriter,
    stems: Vec<WavWriter>,
    /// One per file: the mix, then the stems.
    speed: Vec<Varispeed>,
    /// Pre-roll blocks rendered so far (the arming frame and the warm-up frames).
    pre_blocks: u64,
    /// Simulation time of the next delivered frame after frame 0, nanoseconds.
    sim_ns: u64,
    /// Delivered frames whose block is written.
    frames: u64,
}

impl AudioOut {
    /// Creates the WAV files and starts the mixdown (after [`check`] passed and the sink opened).
    pub fn start(world: &mut World, request: &Request, audio: &AudioRequest) -> Result<Self, String> {
        let mix = WavWriter::create(mix_path(request), audio.format, audio.sample_rate)?;
        let mut stems = Vec::with_capacity(audio.stems.len());
        for bus in &audio.stems {
            match WavWriter::create(stem_path(request, bus), audio.format, audio.sample_rate) {
                Ok(writer) => stems.push(writer),
                Err(error) => {
                    mix.discard();
                    stems.into_iter().for_each(WavWriter::discard);
                    return Err(error);
                }
            }
        }
        let started = world
            .get_resource_mut::<Audio>()
            .ok_or_else(|| "the Audio resource is gone".to_string())
            .and_then(|mut mixer| mixer.begin_mixdown(&audio.mixdown()).map_err(|e| e.to_string()));
        if let Err(error) = started {
            mix.discard();
            stems.into_iter().for_each(WavWriter::discard);
            return Err(error);
        }
        Ok(Self {
            request: audio.clone(),
            speed: vec![Varispeed::default(); 1 + stems.len()],
            mix,
            stems,
            pre_blocks: 0,
            sim_ns: 0,
            frames: 0,
        })
    }

    fn render(world: &mut World, frames: u64) -> Result<regia_audio::MixdownBlock, String> {
        let mut mixer = world
            .get_resource_mut::<Audio>()
            .ok_or("the Audio resource is gone")?;
        mixer
            .render_mixdown(frames as usize)
            .map_err(|e| format!("audio mixdown: {e}"))
    }

    /// Renders and drops one pre-roll block (an arming or warm-up frame): the warm-up step on the
    /// timeline's clock (the render step, or the simulation step into frame 0), so the mixer's
    /// clock reaches frame 0 with the job's.
    pub fn pre_roll(&mut self, world: &mut World, request: &Request) -> Result<(), String> {
        let _span = tracing::debug_span!(target: "regia_agent", "render_out_audio_preroll").entered();
        let step = if self.request.timeline.follows_sim() {
            request.pre_sim_step_ns()
        } else {
            request.step_ns()
        };
        let k = self.pre_blocks;
        let frames = clock_offset(step.saturating_mul(k + 1), self.request.sample_rate)
            - clock_offset(step.saturating_mul(k), self.request.sample_rate);
        Self::render(world, frames)?;
        self.pre_blocks += 1;
        Ok(())
    }

    /// Renders and writes delivered frame `index`'s block.
    pub fn frame(&mut self, world: &mut World, request: &Request, index: u64) -> Result<(), String> {
        let _span = tracing::debug_span!(target: "regia_agent", "render_out_audio_frame").entered();
        let (mix_frames, out_frames) = frame_samples(request, &self.request, index, self.sim_ns);
        let block = Self::render(world, mix_frames)?;
        let out = out_frames as usize;
        let mixed = self.speed[0].process(&block.mix, out);
        self.mix.write(&mixed)?;
        for (i, (writer, stem)) in self.stems.iter_mut().zip(&block.stems).enumerate() {
            let resampled = self.speed[i + 1].process(stem, out);
            writer.write(&resampled)?;
        }
        self.sim_ns = self.sim_ns.saturating_add(request.sim_step_ns(index));
        self.frames = index + 1;
        Ok(())
    }

    /// The running job's `audio` block for `render.out_progress`.
    pub fn progress_json(&self, request: &Request) -> Value {
        json!({
            "sample_rate": self.request.sample_rate,
            "format": self.request.format.name(),
            "timeline": self.request.timeline.name(),
            "clock": self.request.timeline.clock(),
            "frames": self.frames,
            "samples": self.mix.frames,
            "file": request.shown(&mix_path(request)),
            "stems": self.request.stems.iter().map(|bus| json!({
                "bus": bus,
                "file": request.shown(&stem_path(request, bus)),
            })).collect::<Vec<_>>(),
        })
    }

    /// Ends the mixdown (on the app thread, with the world) and leaves the WAVs to close.
    pub fn end(self, world: &mut World) -> AudioClose {
        if let Some(mut mixer) = world.get_resource_mut::<Audio>() {
            mixer.end_mixdown();
        }
        AudioClose {
            request: self.request,
            mix: self.mix,
            stems: self.stems,
            frames: self.frames,
        }
    }
}

/// The WAVs of a job whose mixdown ended, waiting for [`Self::close`].
pub struct AudioClose {
    request: AudioRequest,
    mix: WavWriter,
    stems: Vec<WavWriter>,
    /// Delivered frames whose block is written.
    frames: u64,
}

impl AudioClose {
    /// Cuts every WAV to the first `keep` delivered frames, writes the headers, syncs the files and
    /// renames them into place. Returns the manifest's `audio` block and the first error. Blocks on
    /// the disk: this runs on the job's closing thread, never on the app thread (ADR-1881).
    pub fn close(self, request: &Request, keep: u64, sync: &RenderOutFileSync) -> (Value, Option<String>) {
        let keep = keep.min(self.frames);
        let keep_ns = if sim_length(request, &self.request) {
            sim_ns_at(request, keep)
        } else {
            0
        };
        let samples = file_offset(request, &self.request, keep, keep_ns);
        let mut error = None;
        let mut done = |writer: WavWriter| match writer.finish(samples, sync) {
            Ok(file) => Some(file),
            Err(e) => {
                if error.is_none() {
                    error = Some(request.scrub(&e));
                }
                None
            }
        };
        let mix = done(self.mix);
        let stems: Vec<Option<WavFile>> = self.stems.into_iter().map(&mut done).collect();
        sync_dir(&request.out_dir, sync);
        let file_json = |file: &Option<WavFile>| match file {
            Some(file) => json!({
                "file": request.shown(&file.path),
                "samples": file.frames,
                "hash": format!("{:016x}", file.hash),
            }),
            None => Value::Null,
        };
        let offsets = if sim_length(request, &self.request) {
            "floor(frame_times[i].sim_time_ns * sample_rate / 1e9)"
        } else {
            "floor(i * sample_rate * fps_den / fps)"
        };
        let mut block = json!({
            "sample_rate": self.request.sample_rate,
            "format": self.request.format.name(),
            "bits": self.request.format.bits(),
            "channels": 2,
            "timeline": self.request.timeline.name(),
            "clock": self.request.timeline.clock(),
            "frames": keep,
            "samples": samples,
            "duration_s": samples as f64 / f64::from(self.request.sample_rate),
            "frame_sample_offset": offsets,
            "mix": file_json(&mix),
            "stems": self.request.stems.iter().zip(&stems).map(|(bus, file)| {
                let mut entry = file_json(file);
                if let Some(object) = entry.as_object_mut() {
                    object.insert("bus".into(), json!(bus));
                }
                entry
            }).collect::<Vec<_>>(),
        });
        if let Some(error) = &error {
            block["error"] = json!(error);
        }
        (block, error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(params: Value) -> Request {
        Request::parse(&params).expect("request")
    }

    #[test]
    fn audio_parses_from_absent_bool_and_object() {
        assert_eq!(AudioRequest::parse(None), Ok(None));
        assert_eq!(AudioRequest::parse(Some(&json!(false))), Ok(None));
        assert_eq!(AudioRequest::parse(Some(&Value::Null)), Ok(None));
        assert_eq!(
            AudioRequest::parse(Some(&json!(true))),
            Ok(Some(AudioRequest::default()))
        );
        let full = AudioRequest::parse(Some(&json!({
            "sample_rate": 44100, "format": "float32", "stems": ["Music", "Sfx"], "timeline": "sim"
        })))
        .expect("valid")
        .expect("on");
        assert_eq!(full.sample_rate, 44_100);
        assert_eq!(full.format, SampleFormat::Float32);
        assert_eq!(full.stems, vec!["Music".to_string(), "Sfx".to_string()]);
        assert_eq!(full.timeline, Timeline::Sim);
        assert_eq!(full.to_json()["bits"], 32);
        assert_eq!(full.to_json()["clock"], "sim");
        assert_eq!(
            AudioRequest::default().timeline,
            Timeline::Render,
            "the render clock by default"
        );
        assert_eq!(AudioRequest::default().to_json()["clock"], "render");
    }

    #[test]
    fn bad_audio_requests_are_refused_with_the_rule() {
        for (value, needle) in [
            (json!("yes"), "must be true, false or an object"),
            (json!({ "rate": 48000 }), "unknown `audio` key `rate`"),
            (json!({ "sample_rate": 4000 }), "sample_rate"),
            (json!({ "sample_rate": 48000.5 }), "sample_rate"),
            (
                json!({ "sample_rate": 4_294_967_296_u64 + 48_000 }),
                "sample_rate",
            ),
            (json!({ "format": "pcm32" }), "audio.format"),
            (json!({ "timeline": "wall" }), "audio.timeline"),
            // ADR-1520's `game` never shipped; it is `sim` now (ADR-1554).
            (json!({ "timeline": "game" }), "audio.timeline"),
            (json!({ "stems": "Music" }), "array of bus names"),
            (json!({ "stems": ["../x"] }), "bus names of"),
            (json!({ "stems": [".hidden"] }), "bus names of"),
            (json!({ "stems": [""] }), "bus names of"),
            (json!({ "stems": [3] }), "bus names of"),
            (json!({ "stems": ["Sfx", "Sfx"] }), "twice"),
        ] {
            let error = AudioRequest::parse(Some(&value)).expect_err("refused");
            assert!(error.contains(needle), "{value}: {error}");
        }
        let many: Vec<String> = (0..=MAX_MIXDOWN_STEMS).map(|i| format!("b{i}")).collect();
        assert!(AudioRequest::parse(Some(&json!({ "stems": many }))).is_err());
    }

    #[test]
    fn output_offsets_are_exact_at_ntsc_rates() {
        // 29.97 fps at 48 kHz: 1601.6 samples a frame, never drifting.
        assert_eq!(output_offset(0, 48_000, 30_000, 1001), 0);
        assert_eq!(output_offset(5, 48_000, 30_000, 1001), 8_008);
        assert_eq!(output_offset(30_000, 48_000, 30_000, 1001), 48_048_000);
        assert_eq!(output_offset(1, 44_100, 24, 1), 1_837);
        assert_eq!(output_offset(24, 44_100, 24, 1), 44_100);
        // The per-frame counts sum to the offset of the end, whatever the rounding.
        let r =
            request(json!({ "out": "/tmp/x", "frames": 1000, "fps": 30000, "fps_den": 1001, "audio": true }));
        let audio = r.audio.clone().expect("audio");
        let mut sum = 0;
        for i in 0..r.frames {
            let (mix, out) = frame_samples(&r, &audio, i, 0);
            assert_eq!(mix, out, "no time map: nothing is resampled");
            assert!((1601..=1602).contains(&out), "frame {i}: {out}");
            sum += out;
        }
        assert_eq!(sum, output_offset(1000, 48_000, 30_000, 1001));
        assert_eq!(total_samples(&r, &audio), sum);
    }

    #[test]
    fn the_wav_is_exactly_duration_times_rate_on_every_timeline_without_a_map() {
        // ADR-1656: a whole number of seconds is exactly `seconds * rate` samples (never one
        // short), and any frame count is `floor(frames * rate * fps_den / fps)`, on all three
        // timelines, with and without a map of ones, at integer and non-integer samples a frame.
        for rate in [8_000_u32, 22_050, 24_000, 44_100, 48_000, 96_000] {
            for (fps, den) in [
                (24_u32, 1_u32),
                (25, 1),
                (30, 1),
                (60, 1),
                (30_000, 1_001),
                (24_000, 1_001),
            ] {
                for frames in [1_u64, 7, 24, 100, 240, 1_001] {
                    for map in [json!(null), json!([{ "frame": 0, "scale": 1.0 }])] {
                        let mut params = json!({
                            "out": "/tmp/x", "frames": frames, "fps": fps, "fps_den": den,
                            "audio": { "sample_rate": rate }
                        });
                        if !map.is_null() {
                            params["time_map"] = map.clone();
                        }
                        let r = request(params);
                        let base = r.audio.clone().expect("audio");
                        let want = (u128::from(frames) * u128::from(rate) * u128::from(den) / u128::from(fps))
                            as u64;
                        for timeline in [Timeline::Render, Timeline::Output, Timeline::Sim] {
                            let audio = AudioRequest {
                                timeline,
                                ..base.clone()
                            };
                            let mut sum = 0;
                            for i in 0..frames {
                                let (mix, out) = frame_samples(&r, &audio, i, 0);
                                assert_eq!(mix, out);
                                sum += out;
                            }
                            let label = format!("{rate} Hz {fps}/{den} fps {frames} frames {timeline:?}");
                            assert_eq!(total_samples(&r, &audio), want, "{label}");
                            assert_eq!(sum, want, "{label}");
                        }
                    }
                }
            }
        }
        // One second at 24 fps and 24 kHz: 24 000, not 23 999.
        let r =
            request(json!({ "out": "/tmp/x", "frames": 24, "fps": 24, "audio": { "sample_rate": 24000 } }));
        let audio = AudioRequest {
            timeline: Timeline::Sim,
            ..r.audio.clone().expect("audio")
        };
        assert_eq!(total_samples(&r, &audio), 24_000);
    }

    #[test]
    fn a_mapped_sim_wav_is_the_floor_of_the_simulation_clock_and_its_blocks_sum_to_it() {
        // `timeline: "sim"` under a real map follows `sim_time_ns` (nanosecond steps, ADR-1524),
        // so its length is `floor(sim_ns * rate / 1e9)`; the blocks add up to exactly that.
        for (fps, scale) in [(24_u32, 0.5_f64), (30, 0.3), (25, 2.0)] {
            let r = request(json!({
                "out": "/tmp/x", "frames": 48, "fps": fps,
                "time_map": [{ "frame": 0, "scale": scale }],
                "audio": { "sample_rate": 48000, "timeline": "sim" }
            }));
            let audio = r.audio.clone().expect("audio");
            let (mut sim_ns, mut sum) = (0, 0);
            for i in 0..r.frames {
                let (mix, out) = frame_samples(&r, &audio, i, sim_ns);
                assert_eq!(mix, out);
                sum += out;
                sim_ns += r.sim_step_ns(i);
            }
            assert_eq!(sum, clock_offset(sim_ns, 48_000), "{fps} fps x{scale}");
            assert_eq!(total_samples(&r, &audio), sum, "{fps} fps x{scale}");
        }
    }

    #[test]
    fn a_time_map_changes_the_mixer_block_only_on_the_simulation_clock() {
        let r = request(json!({
            "out": "/tmp/x", "frames": 10, "fps": 30,
            "time_map": [{ "frame": 0, "scale": 0.5 }],
            "audio": { "sample_rate": 48000 }
        }));
        let audio = r.audio.clone().expect("audio");
        assert_eq!(audio.timeline, Timeline::Render);
        let output = AudioRequest {
            timeline: Timeline::Output,
            ..audio.clone()
        };
        let mut sim_ns = 0;
        for i in 0..r.frames {
            assert_eq!(
                frame_samples(&r, &audio, i, sim_ns),
                (1_600, 1_600),
                "the render clock keeps 1/30 s a frame, map or not"
            );
            let (mix, out) = frame_samples(&r, &output, i, sim_ns);
            assert_eq!(out, 1_600, "the output timeline keeps 1/30 s a frame");
            assert_eq!(mix, 800, "half the simulation time: half the mixer block");
            sim_ns += r.sim_step_ns(i);
        }
        assert_eq!(total_samples(&r, &audio), 16_000);
        assert_eq!(total_samples(&r, &output), 16_000);
        let sim = AudioRequest {
            timeline: Timeline::Sim,
            ..audio
        };
        assert_eq!(frame_samples(&r, &sim, 3, 3 * r.sim_step_ns(0)), (800, 800));
        assert_eq!(total_samples(&r, &sim), 8_000);
    }

    #[test]
    fn the_wav_size_limit_is_checked_at_parse() {
        // 1,000,000 frames at 1 fps of float32 at 192 kHz is far over 4 GiB.
        let error = Request::parse(&json!({
            "out": "/tmp/x", "frames": 1_000_000, "fps": 1,
            "audio": { "sample_rate": 192000, "format": "float32" }
        }))
        .expect_err("too long");
        assert!(error.message.contains("4 GiB"), "{}", error.message);
        // One hour of 16-bit 48 kHz stereo is 691 MB: fine.
        assert!(
            Request::parse(&json!({
                "out": "/tmp/x", "frames": 108_000, "fps": 30, "audio": { "format": "pcm16" }
            }))
            .is_ok()
        );
    }

    #[test]
    fn varispeed_keeps_the_length_and_copies_equal_blocks() {
        let mut v = Varispeed::default();
        let input: Vec<f32> = (0..20).map(|i| i as f32 / 20.0).collect();
        assert_eq!(v.process(&input, 10), input, "the same length is a copy");
        for frames in [1, 3, 7, 10, 13, 40] {
            let mut v = Varispeed::default();
            assert_eq!(v.process(&input, frames).len(), frames * 2, "{frames}");
        }
        let mut v = Varispeed::default();
        assert_eq!(v.process(&[], 5), vec![0.0; 10], "frozen time is silence");
        assert!(
            v.process(&[], 0).is_empty(),
            "an empty block for zero frames must not panic"
        );
        assert!(v.process(&[0.5, 0.5], 0).is_empty(), "zero output frames");
    }

    #[test]
    fn varispeed_slows_down_by_interpolating_from_the_last_block() {
        let mut v = Varispeed::default();
        // A ramp 0, 1, 2, 3 (left) and its negation (right), then the same block stretched x2.
        let ramp: Vec<f32> = (0..4).flat_map(|i| [i as f32, -(i as f32)]).collect();
        assert_eq!(v.process(&ramp, 4), ramp);
        let next: Vec<f32> = (4..8).flat_map(|i| [i as f32, -(i as f32)]).collect();
        let out = v.process(&next, 8);
        let left: Vec<f32> = out.iter().step_by(2).copied().collect();
        // From the previous block's 3 to this block's 7 in half steps, ending on the last input.
        assert_eq!(left, vec![3.5, 4.0, 4.5, 5.0, 5.5, 6.0, 6.5, 7.0]);
        assert_eq!(out[1], -3.5);
    }

    #[test]
    fn varispeed_speeds_up_by_averaging_what_each_sample_covers() {
        let mut v = Varispeed::default();
        let input: Vec<f32> = [1.0, 3.0, 5.0, 7.0, 9.0, 11.0]
            .iter()
            .flat_map(|x| [*x, *x])
            .collect();
        // 6 frames into 3: pairs averaged.
        let out = v.process(&input, 3);
        assert_eq!(out, vec![2.0, 2.0, 6.0, 6.0, 10.0, 10.0]);
        // 6 into 4: each output covers 1.5 inputs.
        let mut v = Varispeed::default();
        let out = v.process(&input, 4);
        let left: Vec<f32> = out.iter().step_by(2).copied().collect();
        let expect = [
            (1.0 + 0.5 * 3.0) / 1.5,
            (0.5 * 3.0 + 5.0) / 1.5,
            (7.0 + 0.5 * 9.0) / 1.5,
            (0.5 * 9.0 + 11.0) / 1.5,
        ];
        for (got, want) in left.iter().zip(expect) {
            assert!((got - want).abs() < 1e-6, "{got} vs {want}");
        }
        // A constant stays constant at every ratio (the weights sum to one).
        let flat = [0.25_f32; 2 * 97];
        for frames in [1, 10, 50, 96] {
            let mut v = Varispeed::default();
            assert!(v.process(&flat, frames).iter().all(|s| (s - 0.25).abs() < 1e-6));
        }
    }

    #[test]
    fn samples_quantize_with_clamping_in_every_format() {
        let mut out = Vec::new();
        SampleFormat::Pcm16.push(1.5, &mut out);
        SampleFormat::Pcm16.push(-1.0, &mut out);
        SampleFormat::Pcm16.push(f32::NAN, &mut out);
        assert_eq!(out, [0xff, 0x7f, 0x01, 0x80, 0x00, 0x00]);
        out.clear();
        SampleFormat::Pcm24.push(1.0, &mut out);
        SampleFormat::Pcm24.push(-1.0, &mut out);
        assert_eq!(out, [0xff, 0xff, 0x7f, 0x01, 0x00, 0x80]);
        out.clear();
        SampleFormat::Float32.push(0.5, &mut out);
        assert_eq!(out, 0.5_f32.to_le_bytes());
    }

    #[test]
    fn wav_headers_follow_the_riff_layout() {
        let h = wav_header(SampleFormat::Pcm24, 48_000, 10);
        assert_eq!(h.len(), 44);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes([h[4], h[5], h[6], h[7]]), 36 + 60);
        assert_eq!(&h[8..16], b"WAVEfmt ");
        assert_eq!(u16::from_le_bytes([h[20], h[21]]), 1, "integer PCM");
        assert_eq!(u16::from_le_bytes([h[22], h[23]]), 2, "stereo");
        assert_eq!(u32::from_le_bytes([h[24], h[25], h[26], h[27]]), 48_000);
        assert_eq!(u32::from_le_bytes([h[28], h[29], h[30], h[31]]), 48_000 * 6);
        assert_eq!(u16::from_le_bytes([h[32], h[33]]), 6, "block align");
        assert_eq!(u16::from_le_bytes([h[34], h[35]]), 24);
        assert_eq!(&h[36..40], b"data");
        assert_eq!(u32::from_le_bytes([h[40], h[41], h[42], h[43]]), 60);
        let f = wav_header(SampleFormat::Float32, 44_100, 3);
        assert_eq!(f.len(), 58);
        assert_eq!(u16::from_le_bytes([f[20], f[21]]), 3, "IEEE float");
        assert_eq!(&f[38..42], b"fact");
        assert_eq!(u32::from_le_bytes([f[46], f[47], f[48], f[49]]), 3);
        assert_eq!(&f[50..54], b"data");
        assert_eq!(u32::from_le_bytes([f[54], f[55], f[56], f[57]]), 24);
    }

    #[test]
    fn a_wav_is_written_to_a_part_file_and_cut_to_the_kept_frames() {
        let dir = regia_test_utils::unique_temp_dir("render-out-wav");
        let path = dir.join("cut.wav");
        let mut writer = WavWriter::create(path.clone(), SampleFormat::Pcm16, 8_000).expect("create");
        writer.write(&[0.5, -0.5, 0.25, -0.25]).expect("write");
        writer.write(&[1.0, 1.0]).expect("write");
        assert!(!path.exists(), "only the part file exists while writing");
        let file = writer.finish(2, &RenderOutFileSync::default()).expect("finish");
        assert_eq!(file.frames, 2);
        let bytes = std::fs::read(&path).expect("read");
        assert_eq!(bytes.len(), 44 + 8);
        assert_eq!(&bytes[..44], wav_header(SampleFormat::Pcm16, 8_000, 2).as_slice());
        assert_eq!(file.hash, fnv1a_extend(FNV_START, &bytes[44..]));
        assert_eq!(file.hash, super::super::plan::fnv1a(&bytes[44..]));
        assert!(!part_path(&path).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A world with a headless mixer and nothing else (no renderer).
    fn mixer_world() -> World {
        let mut world = World::new();
        world.insert_resource(Audio::new(&regia_audio::AudioSettings {
            headless: true,
            ..regia_audio::AudioSettings::default()
        }));
        world
    }

    /// A 0.05 s 1 kHz tone, 48 kHz mono.
    fn tone() -> regia_audio::SoundAsset {
        let samples: Vec<f32> = (0..2_400)
            .map(|i| ((i as f32 / 48_000.0) * 1_000.0 * std::f32::consts::TAU).sin() * 0.5)
            .collect();
        regia_audio::SoundAsset::from_clip(&regia_audio::SoundClip::new(samples, 48_000).expect("tone"))
    }

    /// Runs the audio side of a job of `frames` frames with the tone played on `play_on`, and
    /// returns the manifest block and the mix's samples (16-bit).
    fn run_job(dir: &Path, params: Value, play_on: u64) -> (Value, Vec<i16>) {
        let r = {
            let mut params = params;
            params["out"] = json!(dir.display().to_string());
            Request::parse(&params).expect("request")
        };
        let audio = r.audio.clone().expect("audio");
        let mut world = mixer_world();
        check(&world, &audio).expect("checked");
        std::fs::create_dir_all(&r.out_dir).expect("out dir");
        let mut out = AudioOut::start(&mut world, &r, &audio).expect("start");
        assert!(world.resource::<Audio>().mixdown_active());
        for _ in 0..=r.warmup {
            out.pre_roll(&mut world, &r).expect("pre-roll");
        }
        for index in 0..r.frames {
            if index == play_on {
                let mut mixer = world.resource_mut::<Audio>();
                mixer
                    .play_asset(&tone(), regia_audio::PlaybackParams::default())
                    .expect("play");
                mixer.drain();
            }
            // The app ticks `Audio` on `Time::delta`: the render clock.
            world.resource_mut::<Audio>().tick(r.step_ns() as f64 * 1e-9);
            out.frame(&mut world, &r, index).expect("frame");
        }
        let (block, error) = out
            .end(&mut world)
            .close(&r, r.frames, &RenderOutFileSync::default());
        assert_eq!(error, None);
        assert!(!world.resource::<Audio>().mixdown_active(), "the output is back");
        let bytes = std::fs::read(r.out_dir.join(format!("{}_audio.wav", r.name))).expect("wav");
        let samples = bytes[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b))
            .collect();
        (block, samples)
    }

    #[test]
    fn a_job_writes_a_wav_as_long_as_the_frames_with_the_sound_on_its_frame() {
        let dir = regia_test_utils::unique_temp_dir("render-out-audio-job");
        let params = json!({
            "frames": 12, "fps": 30, "warmup_frames": 2, "name": "shot",
            "audio": { "format": "pcm16", "stems": ["Master", "Music"] }
        });
        let (block, samples) = run_job(&dir, params.clone(), 5);
        assert_eq!(block["samples"], 12 * 1_600);
        assert_eq!(samples.len(), 2 * 12 * 1_600);
        assert_eq!(block["mix"]["samples"], 12 * 1_600);
        assert_eq!(block["stems"][0]["bus"], "Master");
        assert_eq!(block["stems"][1]["samples"], 12 * 1_600);
        // Silent before frame 5, the tone from frame 5's first sample on.
        let onset = samples.iter().position(|s| *s != 0).expect("the tone is heard") / 2;
        assert!(
            (5 * 1_600..5 * 1_600 + 48).contains(&onset),
            "onset at sample {onset}, frame 5 starts at 8000"
        );
        // A second run gives the same bytes.
        let dir2 = regia_test_utils::unique_temp_dir("render-out-audio-job");
        let (again, samples2) = run_job(&dir2, params, 5);
        assert_eq!(samples, samples2);
        assert_eq!(block["mix"]["hash"], again["mix"]["hash"]);
        assert_eq!(block["stems"][1]["hash"], again["stems"][1]["hash"]);
        for d in [dir, dir2] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn a_time_map_keeps_the_video_length_or_writes_the_sim_length() {
        for (timeline, per_frame) in [("render", 1_600_u64), ("output", 1_600), ("sim", 800)] {
            let dir = regia_test_utils::unique_temp_dir("render-out-audio-timeline");
            let params = json!({
                "frames": 6, "fps": 30, "warmup_frames": 1,
                "time_map": [{ "frame": 0, "scale": 0.5 }],
                "audio": { "format": "pcm16", "timeline": timeline }
            });
            let (block, samples) = run_job(&dir, params, 2);
            assert_eq!(block["timeline"], timeline);
            let clock = if timeline == "render" { "render" } else { "sim" };
            assert_eq!(block["clock"], clock);
            assert_eq!(block["samples"], 6 * per_frame, "{timeline}");
            assert_eq!(samples.len() as u64, 2 * 6 * per_frame, "{timeline}");
            // The tone starts on frame 2 of its timeline (varispeed's interpolation reaches into the
            // frame before by at most one sample).
            let onset = samples.iter().position(|s| *s != 0).expect("the tone is heard") as u64 / 2;
            assert!(
                (2 * per_frame - 1..2 * per_frame + 48).contains(&onset),
                "{timeline}: onset {onset}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn the_render_clock_sound_does_not_move_with_the_time_map() {
        // Sounds on the render clock keep their speed and pitch in slow motion: the WAV under a
        // 0.25 map is the WAV without a map, sample for sample (ADR-1554).
        let run = |map: Value| {
            let dir = regia_test_utils::unique_temp_dir("render-out-audio-render-clock");
            let mut params = json!({
                "frames": 8, "fps": 30, "warmup_frames": 2,
                "audio": { "format": "pcm16", "stems": ["Master"] }
            });
            if !map.is_null() {
                params["time_map"] = map;
            }
            let result = run_job(&dir, params, 3);
            let _ = std::fs::remove_dir_all(&dir);
            result
        };
        let (plain, plain_samples) = run(Value::Null);
        let (slow, slow_samples) = run(json!([{ "frame": 0, "scale": 0.25 }]));
        assert_eq!(plain_samples, slow_samples);
        assert_eq!(plain["mix"]["hash"], slow["mix"]["hash"]);
        assert_eq!(plain["stems"][0]["hash"], slow["stems"][0]["hash"]);
        assert_eq!(
            slow["frame_sample_offset"],
            "floor(i * sample_rate * fps_den / fps)"
        );
        assert_eq!(block_files(&slow).len(), 2, "the mix and one stem");
    }

    #[test]
    fn block_files_lists_the_mix_then_the_stems_and_skips_failed_ones() {
        let block = json!({
            "mix": { "file": "out/a_audio.wav" },
            "stems": [{ "bus": "Sfx", "file": "out/a_audio_Sfx.wav" }, null],
        });
        assert_eq!(
            block_files(&block),
            vec![json!("out/a_audio.wav"), json!("out/a_audio_Sfx.wav")]
        );
        assert!(block_files(&Value::Null).is_empty());
    }

    #[test]
    fn finishing_early_cuts_the_wav_to_the_kept_frames() {
        let dir = regia_test_utils::unique_temp_dir("render-out-audio-cut");
        let r = Request::parse(&json!({
            "out": dir.display().to_string(), "frames": 6, "fps": 24, "audio": { "sample_rate": 44100 }
        }))
        .expect("request");
        let audio = r.audio.clone().expect("audio");
        let mut world = mixer_world();
        std::fs::create_dir_all(&r.out_dir).expect("out dir");
        let mut out = AudioOut::start(&mut world, &r, &audio).expect("start");
        out.pre_roll(&mut world, &r).expect("pre-roll");
        for index in 0..4 {
            out.frame(&mut world, &r, index).expect("frame");
        }
        assert_eq!(out.progress_json(&r)["frames"], 4);
        assert_eq!(out.progress_json(&r)["samples"], output_offset(4, 44_100, 24, 1));
        // Only three frames made it to disk: the WAV keeps three.
        let (block, error) = out.end(&mut world).close(&r, 3, &RenderOutFileSync::default());
        assert_eq!(error, None);
        assert_eq!(block["frames"], 3);
        assert_eq!(block["samples"], output_offset(3, 44_100, 24, 1));
        let len = std::fs::metadata(r.out_dir.join("frame_audio.wav"))
            .expect("wav")
            .len();
        assert_eq!(len, 44 + output_offset(3, 44_100, 24, 1) * 6);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_refuses_an_app_without_audio_and_unknown_stems() {
        let world = World::new();
        let error = check(&world, &AudioRequest::default()).expect_err("no mixer");
        assert_eq!(error.code, protocol::UNSUPPORTED);
        assert_eq!(
            error.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("F0013")
        );
        let world = mixer_world();
        let unknown = AudioRequest {
            stems: vec!["Nope".into()],
            ..AudioRequest::default()
        };
        let error = check(&world, &unknown).expect_err("unknown bus");
        assert_eq!(error.code, protocol::INVALID_PARAMS);
        assert!(error.message.contains("Nope"), "{}", error.message);
        assert!(check(&world, &AudioRequest::default()).is_ok());
    }
}
