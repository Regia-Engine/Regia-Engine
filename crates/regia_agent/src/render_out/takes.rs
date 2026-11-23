// SPDX-License-Identifier: MPL-2.0
//! Sequence-driven render-out (ADR-1616): a `.render_job.ron` resolved into one timeline from
//! sequence time 0 with its takes, cut list and output paths ([`plan`], pure), and the parts of a
//! running job that write each frame into the takes holding it ([`TakeRun`]) and drive the shot
//! player on the render clock ([`drive_job_shots`]).
//!
//! The timeline is rendered from frame 0 whatever the takes are, and a frame no take holds is
//! rendered and not written (the pre-roll). So a take's frames are the same frames, byte for byte,
//! as in a render of the whole timeline: nothing about a frame depends on which takes write it.

use std::path::PathBuf;

use regia_app::prelude::*;
use regia_framework::sequencer::{
    Binding, ClockSource, EndPolicy, Sequence, SequenceBindings, SequenceLibrary, SequencePlayer,
};
use regia_framework::shots::{ShotPlayer, ShotSequence};
use serde_json::{Value, json};

use super::plan::{MAX_FRAMES, Request, confine_out_dir};
use super::sink::{Finished, Sink};
use super::writer::{AovFile, FrameData};
use super::{Phase, RenderOutJobs};
use crate::protocol::{self, RpcError};
use crate::render_job::{
    CutAt, JobBinding, OutputSettings, RenderJob, TakeRange, is_valid_date, is_valid_name,
};

/// The plan's warning when a job names a `.seq.ron`: its markers are used, its tracks are not
/// played (the sequence player is a later lane).
pub(crate) const WARN_SEQUENCE_TRACKS: &str = "sequence_tracks_not_applied";
/// The plan's warning when a job's binding table leaves targets of its sequence out: they do not
/// animate (`unbound_targets` in the plan lists them).
pub(crate) const WARN_UNBOUND_TARGETS: &str = "sequence_targets_unbound";
/// The plan's warning when an explicit cut lies at or past the end of the timeline (dropped).
pub(crate) const WARN_CUT_PAST_END: &str = "cut_past_end";

/// A refused job: a protocol error code (`book/agents/render-jobs.md`) and the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanError {
    /// The lowercase error code.
    pub code: &'static str,
    /// What is wrong, naming the take, marker or shot.
    pub message: String,
}

fn fail<T>(code: &'static str, message: impl Into<String>) -> Result<T, PlanError> {
    Err(PlanError {
        code,
        message: message.into(),
    })
}

/// The files a job's takes and cuts are resolved against.
#[derive(Clone, Copy, Default)]
pub(crate) struct Sources<'a> {
    /// The job's `.shots.ron`.
    pub shots: Option<&'a ShotSequence>,
    /// The job's `.seq.ron` (markers only).
    pub sequence: Option<&'a Sequence>,
    /// A product job's timeline end, from its timed events (`Whole` with no file to go by).
    pub product_end: Option<u64>,
}

/// One stretch of a take between two cuts, and the shot it shows.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Segment {
    /// First timeline frame.
    pub start: u64,
    /// Timeline frame after the last.
    pub end: u64,
    /// The shot at `start` (`None` without a shot list).
    pub shot: Option<String>,
}

/// A take resolved to timeline frames and an output path.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlannedTake {
    /// The take's name.
    pub name: String,
    /// First timeline frame.
    pub start: u64,
    /// Timeline frame after the last.
    pub end: u64,
    /// Folders under the job's `out`.
    pub dirs: Vec<String>,
    /// File stem of the take's frames.
    pub stem: String,
    /// The shot at `start` (`None` without a shot list).
    pub first_shot: Option<String>,
    /// The job's cut frames inside the take (`start < cut < end`), timeline frames.
    pub cuts: Vec<u64>,
    /// The take split at its cuts.
    pub segments: Vec<Segment>,
}

impl PlannedTake {
    /// Frames the take writes.
    pub fn len(&self) -> u64 {
        self.end - self.start
    }

    /// True when timeline frame `frame` belongs to the take.
    pub fn holds(&self, frame: u64) -> bool {
        (self.start..self.end).contains(&frame)
    }

    /// The folder path under `out`, `/`-separated (`.` for none).
    pub fn dir_text(&self) -> String {
        if self.dirs.is_empty() {
            ".".into()
        } else {
            self.dirs.join("/")
        }
    }
}

/// A job resolved against its sources: the timeline length, its cut frames and the takes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TakePlan {
    /// The job name.
    pub job: String,
    /// Timeline frames rendered: the latest take end.
    pub frames: u64,
    /// The timeline's frame rate.
    pub rate: Rate,
    /// Every cut frame of the timeline (shot boundaries and the job's cuts), sorted, without 0.
    pub cuts: Vec<u64>,
    /// The takes, in job order.
    pub takes: Vec<PlannedTake>,
    /// Timeline frames rendered and written to no take.
    pub preroll: u64,
    /// Warnings (`WARN_*`).
    pub warnings: Vec<&'static str>,
    /// Sequence players on the external clock in the world when the job was planned (filled by
    /// `render.out_job`; 0 in a pure plan). It counts the player a binding table creates.
    pub sequence_players: usize,
    /// The job's binding table resolved: each target name and the entity it binds (v2, ADR-1880;
    /// empty without one).
    pub bindings: Vec<(String, u64)>,
    /// Targets of the sequence the binding table leaves out: they do not animate (v2, ADR-1880).
    pub unbound_targets: Vec<String>,
    /// The product a job renders instead of the world (v3, ADR-2757); `null` without one.
    pub product: Value,
}

/// A frame rate as the request gives it: `num / den` frames per second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rate {
    /// Numerator (frames).
    pub num: u64,
    /// Denominator (seconds).
    pub den: u64,
}

impl Rate {
    /// The request's frame rate.
    pub fn of(request: &Request) -> Self {
        Self {
            num: u64::from(request.fps_num),
            den: u64::from(request.fps_den),
        }
    }
}

/// Sequence time of timeline frame `frame`, seconds: `frame · den / num`, one rounding. The plan
/// and the player seek use this one conversion, so a shot change lands on the frame the player
/// shows it on, and frame 30 at 30 fps is exactly 1 s (the render clock's whole-nanosecond step
/// would put it at 0.99999999 s and move every cut a frame late).
pub(crate) fn frame_seconds(frame: u64, rate: Rate) -> f64 {
    frame.saturating_mul(rate.den) as f64 / rate.num.max(1) as f64
}

/// The first timeline frame whose time is `t` seconds or later (half-open ranges start and end
/// there). A time past [`MAX_FRAMES`] frames gives `MAX_FRAMES + 1`, which the plan refuses.
pub(crate) fn frame_at(t: f64, rate: Rate) -> u64 {
    if t.is_nan() || t <= 0.0 {
        return 0;
    }
    let guess = (t * rate.num as f64 / rate.den.max(1) as f64).ceil();
    if guess > (MAX_FRAMES + 1) as f64 {
        return MAX_FRAMES + 1;
    }
    let mut frame = guess as u64;
    // The guess is off by at most a frame of rounding either way.
    while frame > 0 && frame_seconds(frame - 1, rate) >= t {
        frame -= 1;
    }
    while frame <= MAX_FRAMES && frame_seconds(frame, rate) < t {
        frame += 1;
    }
    frame
}

/// The shot shown at timeline frame `frame`.
fn shot_of(shots: &ShotSequence, frame: u64, rate: Rate) -> usize {
    shots
        .locate(frame_seconds(frame, rate))
        .map_or(0, |(index, _)| index)
}

/// The first timeline frame that shows shot `index` or a later one (shot `index`'s cut frame).
fn shot_start_frame(shots: &ShotSequence, index: usize, rate: Rate) -> u64 {
    if index == 0 {
        return 0;
    }
    let mut frame = frame_at(shots.shot_start(index), rate);
    // `locate` decides; the guess from the summed durations is off by a frame at most.
    for _ in 0..4 {
        if frame > 0 && shot_of(shots, frame - 1, rate) >= index {
            frame -= 1;
        } else if frame <= MAX_FRAMES && shot_of(shots, frame, rate) < index {
            frame += 1;
        } else {
            break;
        }
    }
    frame
}

/// The cut frame of every shot boundary of `shots`, in order (equal frames when a shot is shorter
/// than a frame).
pub(crate) fn shot_cut_frames(shots: &ShotSequence, rate: Rate) -> Vec<u64> {
    (1..shots.shots.len())
        .map(|index| shot_start_frame(shots, index, rate))
        .collect()
}

fn need_shots<'a>(sources: Sources<'a>, what: &str) -> Result<&'a ShotSequence, PlanError> {
    sources.shots.ok_or_else(|| PlanError {
        code: "render_job_no_source",
        message: format!("{what} needs a shot list: name a `.shots.ron` in `shots`"),
    })
}

pub(crate) fn need_sequence<'a>(sources: Sources<'a>, what: &str) -> Result<&'a Sequence, PlanError> {
    sources.sequence.ok_or_else(|| PlanError {
        code: "render_job_no_source",
        message: format!("{what} needs a sequence: name a `.seq.ron` in `sequence`"),
    })
}

pub(crate) fn marker_time(sequence: &Sequence, name: &str, take: &str) -> Result<f64, PlanError> {
    match sequence.marker(name) {
        Some(marker) => Ok(f64::from(marker.time)),
        None => fail(
            "render_job_unknown_marker",
            format!("{take}: the sequence has no marker `{name}`"),
        ),
    }
}

fn shot_index(shots: &ShotSequence, name: &str, take: &str) -> Result<usize, PlanError> {
    match shots.shot_index(name) {
        Some(index) => Ok(index),
        None => fail(
            "render_job_unknown_shot",
            format!("{take}: the shot list has no shot `{name}`"),
        ),
    }
}

/// The timeline frames `start..end` of one take range.
fn resolve_range(
    range: &TakeRange,
    sources: Sources<'_>,
    rate: Rate,
    take: &str,
) -> Result<(u64, u64), PlanError> {
    let label = format!("take `{take}`");
    let (start, end) = match range {
        TakeRange::Whole => match (sources.shots, sources.sequence) {
            (Some(shots), _) => (0, frame_at(shots.duration(), rate)),
            (None, Some(sequence)) => (0, frame_at(sequence.duration(), rate)),
            (None, None) => match sources.product_end {
                Some(end) => (0, end),
                None => {
                    return fail(
                        "render_job_no_source",
                        format!("{label}: `Whole` needs a `shots` or `sequence` file"),
                    );
                }
            },
        },
        TakeRange::Frames { start, end } => (*start, *end),
        TakeRange::Seconds { start, end } => (frame_at(*start, rate), frame_at(*end, rate)),
        TakeRange::Markers { from, to } => {
            let sequence = need_sequence(sources, &format!("{label} (`Markers`)"))?;
            let from = marker_time(sequence, from, &label)?;
            let to = marker_time(sequence, to, &label)?;
            (frame_at(from, rate), frame_at(to, rate))
        }
        TakeRange::Shots { first, last } => {
            let shots = need_shots(sources, &format!("{label} (`Shots`)"))?;
            let a = shot_index(shots, first, &label)?;
            let b = shot_index(shots, last, &label)?;
            if b < a {
                return fail(
                    "render_job_invalid",
                    format!("{label}: shot `{last}` comes before shot `{first}`"),
                );
            }
            let end = if b + 1 < shots.shots.len() {
                shot_start_frame(shots, b + 1, rate)
            } else {
                frame_at(shots.duration(), rate)
            };
            (shot_start_frame(shots, a, rate), end)
        }
    };
    if end <= start {
        return fail(
            "render_job_invalid",
            format!("{label} resolves to frames {start}..{end}, which is empty"),
        );
    }
    if end > MAX_FRAMES {
        return fail(
            "render_job_invalid",
            format!("{label} ends at frame {end}; a job renders at most {MAX_FRAMES} frames"),
        );
    }
    Ok((start, end))
}

/// The values the path tokens of one take expand to.
struct Tokens<'a> {
    job: &'a str,
    take: &'a str,
    shot: Option<&'a str>,
    frame: u64,
    date: Option<&'a str>,
}

/// Expands a path template into folders and a file stem (`render_job_bad_path` on an unknown
/// token, `{shot}` without a shot list, `{date}` without a date, or a part that is not a valid
/// name).
fn expand(template: &str, tokens: &Tokens<'_>) -> Result<(Vec<String>, String), PlanError> {
    let bad = |m: String| PlanError {
        code: "render_job_bad_path",
        message: format!("take `{}`: path `{template}`: {m}", tokens.take),
    };
    let mut text = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(open) = rest.find(['{', '}']) {
        if rest.as_bytes()[open] == b'}' {
            return Err(bad("a `}` without its `{`".into()));
        }
        text.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let close = after
            .find('}')
            .ok_or_else(|| bad("a `{` without its `}`".into()))?;
        let value = match &after[..close] {
            "job" => tokens.job.to_string(),
            "take" => tokens.take.to_string(),
            "frame" => format!("{:06}", tokens.frame),
            "shot" => tokens
                .shot
                .ok_or_else(|| bad("`{shot}` needs a shot list (`shots`)".into()))?
                .to_string(),
            "date" => match tokens.date {
                Some(date) => date.to_string(),
                None => {
                    return Err(PlanError {
                        code: "render_job_no_date",
                        message: format!(
                            "take `{}`: path `{template}` uses `{{date}}` and no `date` was given",
                            tokens.take
                        ),
                    });
                }
            },
            other => {
                return Err(bad(format!(
                    "unknown token `{{{other}}}` (job, take, shot, frame, date)"
                )));
            }
        };
        text.push_str(&value);
        rest = &after[close + 1..];
    }
    text.push_str(rest);
    let mut parts: Vec<String> = text.split('/').map(str::to_string).collect();
    for part in &parts {
        if !is_valid_name(part) {
            return Err(bad(format!(
                "`{part}` is not a valid folder or file name (1..=64 letters, digits, `_`, `-` or `.`, not leading `.`)"
            )));
        }
    }
    let stem = parts.pop().unwrap_or_default();
    Ok((parts, stem))
}

/// Frames in `0..frames` that no range holds.
fn uncovered(frames: u64, ranges: &[(u64, u64)]) -> u64 {
    let mut sorted = ranges.to_vec();
    sorted.sort_unstable();
    let mut covered = 0;
    let mut reach = 0;
    for (start, end) in sorted {
        let start = start.max(reach);
        if end > start {
            covered += end - start;
            reach = end;
        }
    }
    frames - covered.min(frames)
}

/// Resolves `job` against its sources at the timeline frame rate `rate`. `date` fills `{date}`.
pub(crate) fn plan(
    job: &RenderJob,
    sources: Sources<'_>,
    rate: Rate,
    date: Option<&str>,
) -> Result<TakePlan, PlanError> {
    let mut ranges = Vec::with_capacity(job.takes.len());
    for take in &job.takes {
        ranges.push(resolve_range(&take.range, sources, rate, &take.name)?);
    }
    let frames = ranges.iter().map(|(_, end)| *end).max().unwrap_or(0);
    let mut warnings = Vec::new();
    if sources.sequence.is_some() {
        warnings.push(WARN_SEQUENCE_TRACKS);
    }
    let mut cuts: Vec<u64> = sources
        .shots
        .map(|shots| shot_cut_frames(shots, rate))
        .unwrap_or_default();
    for cut in &job.cuts {
        let frame = match cut {
            CutAt::Frame(frame) => *frame,
            CutAt::Seconds(t) => frame_at(*t, rate),
            CutAt::Marker(name) => {
                let sequence = need_sequence(sources, &format!("cut at marker `{name}`"))?;
                frame_at(marker_time(sequence, name, "a cut")?, rate)
            }
        };
        if frame >= frames && !warnings.contains(&WARN_CUT_PAST_END) {
            warnings.push(WARN_CUT_PAST_END);
        }
        cuts.push(frame);
    }
    // Frame 0 is the job's own opening cut.
    cuts.retain(|frame| *frame > 0 && *frame < frames);
    cuts.sort_unstable();
    cuts.dedup();

    let preroll = uncovered(frames, &ranges);
    if let Some(cap) = job.max_preroll_frames
        && preroll > cap
    {
        return fail(
            "render_job_preroll_cap",
            format!(
                "the job renders {preroll} frames that no take writes (takes start after frame 0), above max_preroll_frames {cap}"
            ),
        );
    }

    let shot_name = |frame: u64| {
        sources
            .shots
            .and_then(|shots| shots.shots.get(shot_of(shots, frame, rate)))
            .map(|shot| shot.name.clone())
    };
    let mut takes = Vec::with_capacity(job.takes.len());
    let mut paths = std::collections::BTreeMap::new();
    for (take, (start, end)) in job.takes.iter().zip(ranges) {
        let first_shot = shot_name(start);
        let template = take.path.as_deref().unwrap_or(&job.path);
        let (dirs, stem) = expand(
            template,
            &Tokens {
                job: job.job_name(),
                take: &take.name,
                shot: first_shot.as_deref(),
                frame: start,
                date,
            },
        )?;
        if let Some(other) = paths.insert((dirs.clone(), stem.clone()), take.name.clone()) {
            return fail(
                "render_job_path_clash",
                format!(
                    "takes `{other}` and `{}` both write `{}/{stem}`; give one a path with {{take}}",
                    take.name,
                    dirs.join("/")
                ),
            );
        }
        let take_cuts: Vec<u64> = cuts
            .iter()
            .copied()
            .filter(|cut| *cut > start && *cut < end)
            .collect();
        let bounds: Vec<u64> = std::iter::once(start)
            .chain(take_cuts.iter().copied())
            .chain(std::iter::once(end))
            .collect();
        let segments = bounds
            .windows(2)
            .map(|pair| Segment {
                start: pair[0],
                end: pair[1],
                shot: shot_name(pair[0]),
            })
            .collect();
        takes.push(PlannedTake {
            name: take.name.clone(),
            start,
            end,
            dirs,
            stem,
            first_shot,
            cuts: take_cuts,
            segments,
        });
    }
    Ok(TakePlan {
        job: job.job_name().to_string(),
        frames,
        rate,
        cuts,
        takes,
        preroll,
        warnings,
        sequence_players: 0,
        bindings: Vec::new(),
        unbound_targets: Vec::new(),
        product: Value::Null,
    })
}

impl TakePlan {
    /// Frames written over all takes (a frame two takes hold counts twice).
    pub fn written_frames(&self) -> u64 {
        self.takes.iter().map(PlannedTake::len).sum()
    }

    /// The take's range, cuts and cut list as the plan, the progress and the manifest show it.
    pub fn take_json(&self, take: &PlannedTake) -> Value {
        let seconds = |frame: u64| frame_seconds(frame, self.rate);
        json!({
            "name": take.name,
            "start_frame": take.start,
            "end_frame": take.end,
            "frames": take.len(),
            "start_s": seconds(take.start),
            "end_s": seconds(take.end),
            "dir": take.dir_text(),
            "stem": take.stem,
            "first_shot": take.first_shot,
            "cut_frames": take.cuts,
            "cuts": take.cuts.iter().map(|cut| cut - take.start).collect::<Vec<_>>(),
            "cut_list": take.segments.iter().map(|segment| json!({
                "shot": segment.shot,
                "start_frame": segment.start,
                "end_frame": segment.end,
                "local_start": segment.start - take.start,
                "local_end": segment.end - take.start,
            })).collect::<Vec<_>>(),
        })
    }

    /// The whole plan (`render.out_job` answers it; the manifest's `job` block extends it).
    pub fn to_json(&self) -> Value {
        json!({
            "job": self.job,
            "frames": self.frames,
            "fps": self.rate.num,
            "fps_den": self.rate.den,
            "preroll_frames": self.preroll,
            "written_frames": self.written_frames(),
            "cuts": self.cuts,
            "warnings": self.warnings,
            "sequence_players": self.sequence_players,
            "bindings": self.bindings.iter().map(|(target, entity)| json!({ "target": target, "entity": entity })).collect::<Vec<_>>(),
            "unbound_targets": self.unbound_targets,
            "takes": self.takes.iter().map(|take| self.take_json(take)).collect::<Vec<_>>(),
            "product": self.product,
        })
    }
}

/// One take's output while the job runs.
struct TakeOut {
    /// The take's own request: its folder, stem and length (for its sink).
    request: Request,
    /// Opened on the take's first frame, closed after its last.
    sink: Option<Sink>,
    /// What the closed sink left behind.
    finished: Option<Finished>,
}

/// The shot player's state before a job loaded its shot list (put back at the end).
struct SavedPlayer {
    sequence: ShotSequence,
    time: f64,
    playing: bool,
    looping: bool,
}

/// A job's shot list and the clock origin it is played from.
pub(crate) struct ShotDrive {
    /// The shot list the job plays.
    sequence: ShotSequence,
    /// What the player held before.
    saved: Option<SavedPlayer>,
}

/// The takes of a running job.
pub(crate) struct TakeRun {
    /// The resolved job.
    pub plan: TakePlan,
    outs: Vec<TakeOut>,
    /// The shot list the job drives (`None` without `shots`).
    pub shots: Option<ShotDrive>,
    /// `Time::elapsed` of timeline frame 0, nanoseconds (set when frame 0 is next).
    origin_ns: Option<u64>,
    /// Sequence players on the external clock the job drives (ADR-1614 `ClockSource::External`).
    pub sequence_players: usize,
    /// The player the job's binding table created ([`spawn_job_player`]), removed at the end.
    pub job_player: Option<Entity>,
    /// What the [`SequenceLibrary`] held under each name the job added, restored at the end.
    pub job_library: Vec<(String, Option<Sequence>)>,
}

/// One take's outcome at the end of a job.
pub(crate) struct TakeResult {
    /// `done`, `cancelled`, `failed` or `not_reached`.
    pub status: &'static str,
    /// The files on disk (frames of a frame-file format, else the movie).
    pub files: Vec<PathBuf>,
    /// Frames of the take on disk (frame files), else the frames handed to the movie.
    pub written: u64,
    /// What went wrong while writing, if anything.
    pub error: Option<String>,
}

impl TakeRun {
    /// The take outputs of `plan` under the confined timeline request's `out_dir`.
    pub fn new(plan: TakePlan, timeline: &Request, shots: Option<ShotSequence>) -> Self {
        let outs = plan
            .takes
            .iter()
            .map(|take| {
                let mut request = timeline.clone();
                request.out_dir = take
                    .dirs
                    .iter()
                    .fold(timeline.out_dir.clone(), |dir, part| dir.join(part));
                request.name = take.stem.clone();
                request.frames = take.len();
                request.cuts = take.cuts.iter().map(|cut| cut - take.start).collect();
                TakeOut {
                    request,
                    sink: None,
                    finished: None,
                }
            })
            .collect();
        Self {
            plan,
            outs,
            shots: shots.map(|sequence| ShotDrive {
                sequence,
                saved: None,
            }),
            origin_ns: None,
            sequence_players: 0,
            job_player: None,
            job_library: Vec::new(),
        }
    }

    /// Hands timeline frame `index` to every take holding it (opening a take's output on its
    /// first frame and closing it after its last). A frame no take holds is dropped: it was
    /// rendered only so the frames after it are what a full render makes.
    ///
    /// The wait for a full output ends when `interrupted` says so: the frame comes back
    /// (`Ok(Some(..))`) and no take got it (ADR-2396). Every take's place is reserved before any
    /// take is sent the frame, so a frame is never half handed over.
    pub fn hand_over(
        &mut self,
        index: u64,
        data: FrameData,
        aovs: Vec<AovFile>,
        interrupted: &dyn Fn() -> bool,
    ) -> Result<Option<(FrameData, Vec<AovFile>)>, String> {
        let hits: Vec<usize> = self
            .plan
            .takes
            .iter()
            .enumerate()
            .filter(|(_, take)| take.holds(index))
            .map(|(k, _)| k)
            .collect();
        for (n, k) in hits.iter().enumerate() {
            let take = &self.plan.takes[*k];
            let out = &mut self.outs[*k];
            if out.sink.is_none() && out.finished.is_none() {
                out.sink = Some(Sink::open(&out.request).map_err(|e| format!("take `{}`: {e}", take.name))?);
            }
            let reserved = match out.sink.as_mut() {
                Some(sink) => sink.reserve(interrupted),
                None => Err("the output is closed".into()),
            };
            match reserved {
                Ok(true) => {}
                Ok(false) => {
                    self.release(&hits[..n]);
                    return Ok(Some((data, aovs)));
                }
                Err(e) => {
                    let message = format!("take `{}`: {e}", take.name);
                    self.release(&hits[..n]);
                    return Err(message);
                }
            }
        }
        let mut data = Some(data);
        let mut aovs = Some(aovs);
        for (n, k) in hits.iter().enumerate() {
            let take = &self.plan.takes[*k];
            let out = &mut self.outs[*k];
            let last = n + 1 == hits.len();
            let (frame, files) = if last {
                (
                    data.take().ok_or("the frame was already handed over")?,
                    aovs.take().unwrap_or_default(),
                )
            } else {
                (
                    data.clone().ok_or("the frame was already handed over")?,
                    aovs.clone().unwrap_or_default(),
                )
            };
            let sink = out
                .sink
                .as_mut()
                .ok_or_else(|| format!("take `{}`: the output is closed", take.name))?;
            sink.send_reserved(index - take.start, frame, files)
                .map_err(|e| format!("take `{}`: {e}", take.name))?;
            if index + 1 == take.end
                && let Some(sink) = out.sink.take()
            {
                let finished = sink.finish();
                let error = finished.error.clone();
                out.finished = Some(finished);
                if let Some(error) = error {
                    return Err(format!("take `{}`: {error}", take.name));
                }
            }
        }
        Ok(None)
    }

    /// Gives back the places reserved in the outputs of takes `hits`.
    fn release(&mut self, hits: &[usize]) {
        for k in hits {
            if let Some(sink) = self.outs[*k].sink.as_mut() {
                sink.release_reserved();
            }
        }
    }

    /// Takes being written at timeline frame `index`.
    pub fn writing(&self, index: u64) -> Vec<&str> {
        self.plan
            .takes
            .iter()
            .filter(|take| take.holds(index))
            .map(|take| take.name.as_str())
            .collect()
    }

    /// Closes every open output and reports each take, `delivered` timeline frames in, with the
    /// job's `status` name for a take it stopped inside.
    pub fn finish(&mut self, delivered: u64, status: &'static str) -> Vec<TakeResult> {
        self.outs
            .iter_mut()
            .zip(&self.plan.takes)
            .map(|(out, take)| {
                if let Some(sink) = out.sink.take() {
                    out.finished = Some(sink.finish());
                }
                let handed = delivered.clamp(take.start, take.end) - take.start;
                let Some(finished) = out.finished.as_ref() else {
                    return TakeResult {
                        status: "not_reached",
                        files: Vec::new(),
                        written: 0,
                        error: None,
                    };
                };
                let written = finished.frames.unwrap_or(handed).min(handed);
                let status = if finished.error.is_some() {
                    "failed"
                } else if written >= take.len() {
                    "done"
                } else {
                    status
                };
                TakeResult {
                    status,
                    files: finished.files.clone(),
                    written,
                    error: finished.error.clone(),
                }
            })
            .collect()
    }
}

/// `render.out_job` load: saves what the shot player holds and loads the job's shot list,
/// stopped at 0. The job seeks it from now on ([`drive_job_shots`]).
pub(crate) fn begin_shots(world: &mut World, run: &mut TakeRun) {
    let Some(drive) = run.shots.as_mut() else { return };
    let mut player = world.get_resource_or_insert_with(ShotPlayer::default);
    drive.saved = player.sequence().cloned().map(|sequence| SavedPlayer {
        sequence,
        time: player.time(),
        playing: player.is_playing(),
        looping: player.is_looping(),
    });
    player.load(drive.sequence.clone());
    player.set_looping(false);
    player.take_pending_cut();
}

/// Timeline frame 0 is next: the render clock's time of frame 0 is `elapsed + step_ns` (the
/// clock steps once more before it; a shutter's lead is part of that step and of frame 0's
/// subframes).
pub(crate) fn set_origin(world: &World, run: &mut TakeRun, step_ns: u64) {
    let elapsed = world
        .get_resource::<Time>()
        .map_or(0, |time| time.elapsed().as_nanos() as u64);
    run.origin_ns = Some(elapsed.saturating_add(step_ns));
}

/// The sequence players on the external clock in `world`.
pub(crate) fn external_players(world: &mut World) -> usize {
    let mut players = world.query::<&SequencePlayer>();
    players
        .iter(world)
        .filter(|player| player.clock == ClockSource::External)
        .count()
}

/// Restarts every sequence player on the external clock at 0 (markers at 0 fire on the job's
/// first frame) and counts them: the job writes their time from now on ([`drive_job_sequences`]).
pub(crate) fn begin_sequences(world: &mut World, run: &mut TakeRun) {
    let mut players = world.query::<&mut SequencePlayer>();
    let mut count = 0;
    for mut player in players.iter_mut(world) {
        if player.clock == ClockSource::External {
            player.stop();
            player.play();
            // The pre-roll's sound is thrown away; a section at zero starts with frame 0.
            player.hold_audio = true;
            count += 1;
        }
    }
    run.sequence_players = count;
}

/// Lets the job's sequence players sound: frame 0 is next, or the job is over.
pub(crate) fn release_sequence_audio(world: &mut World) {
    let mut players = world.query::<&mut SequencePlayer>();
    for mut player in players.iter_mut(world) {
        if player.clock == ClockSource::External {
            player.hold_audio = false;
        }
    }
}

/// What a job's binding table plays (v2, ADR-1880): its sequence, the sub-sequences it plays
/// (read from their project paths) and the resolved bindings.
pub(crate) struct JobPlayer {
    /// The job's `.seq.ron`.
    pub sequence: Sequence,
    /// Its sub-sequences, by name (project path).
    pub library: Vec<(String, Sequence)>,
    /// Target name → binding.
    pub bindings: Vec<(String, Binding)>,
}

/// `render.out_job` start with a binding table: adds the job's sub-sequences to the
/// [`SequenceLibrary`] and spawns an external-clock player of the job's sequence with the
/// bindings, stopped (the job restarts it at 0 with every external player, [`begin_sequences`]).
pub(crate) fn spawn_job_player(world: &mut World, run: &mut TakeRun, player: Option<JobPlayer>) {
    let Some(job) = player else { return };
    let mut library = world.get_resource_or_insert_with(SequenceLibrary::default);
    for (name, sequence) in job.library {
        run.job_library.push((name.clone(), library.get(&name).cloned()));
        library.insert(name, sequence);
    }
    let snapshot = library.clone();
    let mut player = SequencePlayer::new(job.sequence);
    player.clock = ClockSource::External;
    player.end = EndPolicy::Hold;
    player.takeover.enabled = false;
    player.set_library(&snapshot);
    let mut bindings = SequenceBindings::default();
    for (target, binding) in job.bindings {
        bindings.bind(target, binding);
    }
    let entity = world
        .spawn((
            player,
            bindings,
            regia_scene::components::Name::new("render_job:sequence"),
        ))
        .id();
    run.job_player = Some(entity);
}

/// Removes the player the job's binding table created and puts the [`SequenceLibrary`] entries
/// it added back to what they were.
pub(crate) fn end_job_player(world: &mut World, run: &mut TakeRun) {
    if let Some(entity) = run.job_player.take()
        && world.get_entity(entity).is_ok()
    {
        world.despawn(entity);
    }
    let restore = std::mem::take(&mut run.job_library);
    if let Some(mut library) = world.get_resource_mut::<SequenceLibrary>() {
        for (name, before) in restore {
            match before {
                Some(sequence) => library.insert(name, sequence),
                None => {
                    library.remove(&name);
                }
            }
        }
    }
}

/// The targets the job's sequence and its sub-sequences bind that the table leaves out.
fn unbound_targets(sequence: &Sequence, library: &[(String, Sequence)], job: &RenderJob) -> Vec<String> {
    let mut tree = SequenceLibrary::default();
    for (name, sub) in library {
        tree.insert(name.clone(), sub.clone());
    }
    sequence
        .target_names(Some(&tree))
        .into_iter()
        .filter(|target| !job.bindings.contains_key(target))
        // A spawnable binds what the player spawns (ADR-2948).
        .filter(|target| !sequence.spawnables.iter().any(|s| &s.binding == target))
        .collect()
}

/// The job's binding table against the world: a name binds the lowest entity of that `Name` (by
/// its guid when it has one), a guid must belong to an entity (`render_job_unbound` otherwise).
fn resolve_job_bindings(world: &mut World, job: &RenderJob) -> Result<Vec<(String, Binding, u64)>, RpcError> {
    use regia_scene::components::{EntityGuid, Name};
    let mut out = Vec::with_capacity(job.bindings.len());
    for (target, binding) in &job.bindings {
        let found = match binding {
            JobBinding::Name(name) => {
                let mut query = world.query::<(Entity, &Name, Option<&EntityGuid>)>();
                query
                    .iter(world)
                    .filter(|(_, n, _)| n.as_str() == name)
                    .min_by_key(|(e, _, _)| e.to_bits())
                    .map(|(e, _, guid)| (guid.map_or(Binding::Entity(e), |g| Binding::Guid(g.0)), e))
            }
            JobBinding::Guid(text) => {
                let guid = text.parse::<regia_core::id::Guid>().ok();
                let mut query = world.query::<(Entity, &EntityGuid)>();
                query
                    .iter(world)
                    .find(|(_, g)| Some(g.0) == guid)
                    .map(|(e, g)| (Binding::Guid(g.0), e))
            }
        };
        let Some((binding, entity)) = found else {
            let what = match binding {
                JobBinding::Name(name) => format!("no entity is named `{name}`"),
                JobBinding::Guid(text) => format!("no entity has guid {text}"),
            };
            return Err(job_error(
                "render_job_unbound",
                format!("binding `{target}`: {what}"),
            ));
        };
        out.push((target.clone(), binding, entity.to_bits()));
    }
    Ok(out)
}

/// Puts back what the shot player held before the job (or clears it).
pub(crate) fn end_shots(world: &mut World, run: &mut TakeRun) {
    let Some(drive) = run.shots.as_mut() else { return };
    let Some(mut player) = world.get_resource_mut::<ShotPlayer>() else {
        return;
    };
    match drive.saved.take() {
        Some(saved) => {
            player.load(saved.sequence);
            player.set_looping(saved.looping);
            player.seek(saved.time);
            if saved.playing {
                player.play();
            }
        }
        None => player.clear(),
    }
}

/// The largest `f64` below `end` (a time still inside the shot that ends at `end`).
fn just_before(end: f64) -> f64 {
    if end > 0.0 && end.is_finite() {
        f64::from_bits(end.to_bits() - 1)
    } else {
        0.0
    }
}

/// The render clock of the job: its constant step and the time of timeline frame 0.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Clock {
    /// The timeline's frame rate.
    pub rate: Rate,
    /// The render clock's step per frame, whole nanoseconds.
    pub step_ns: u64,
    /// `Time::elapsed` of frame 0, nanoseconds (`None` before frame 0 is next).
    pub origin_ns: Option<u64>,
}

/// The timeline time on the render clock's `elapsed_ns` while timeline frame `frame` is being
/// made: the frame's own time ([`frame_seconds`]) plus how far the clock is past that frame's
/// start (a shutter subframe), never below 0. Before frame 0 (no origin yet) it is 0.
pub(crate) fn timeline_seconds(clock: Clock, elapsed_ns: u64, frame: u64) -> f64 {
    let Some(origin) = clock.origin_ns else { return 0.0 };
    let frame_start = i128::from(origin) + i128::from(frame) * i128::from(clock.step_ns);
    let offset_ns = i128::from(elapsed_ns) - frame_start;
    (frame_seconds(frame, clock.rate) + offset_ns as f64 / 1e9).max(0.0)
}

/// The shot-list time the player shows: [`timeline_seconds`] clamped into the shot of `frame`
/// itself, so a shutter never straddles a cut.
pub(crate) fn shot_time(shots: &ShotSequence, clock: Clock, elapsed_ns: u64, frame: u64) -> f64 {
    if clock.origin_ns.is_none() {
        return 0.0;
    }
    let raw = timeline_seconds(clock, elapsed_ns, frame);
    let index = shot_of(shots, frame, clock.rate);
    let low = shots.shot_start(index);
    let high = if index + 1 < shots.shots.len() {
        just_before(shots.shot_start(index + 1))
    } else {
        shots.duration()
    };
    raw.clamp(low, high.max(low))
}

/// `PostUpdate`, before the shot player drives the camera: while a job with a shot list runs,
/// seeks the paused player to the job's time ([`shot_time`]) and drops the player's own cut
/// (the job cuts at every shot boundary itself, with its exposure metering).
pub(crate) fn drive_job_shots(
    time: Res<Time>,
    jobs: Option<Res<RenderOutJobs>>,
    player: Option<ResMut<ShotPlayer>>,
) {
    let (Some(jobs), Some(mut player)) = (jobs, player) else {
        return;
    };
    let Some(job) = jobs.active.as_ref() else { return };
    let Some(run) = job.takes.as_ref() else { return };
    let Some(drive) = run.shots.as_ref() else { return };
    let clock = job_clock(job, run);
    let seconds = shot_time(
        &drive.sequence,
        clock,
        time.elapsed().as_nanos() as u64,
        job.delivered,
    );
    if player.is_playing() {
        player.pause();
    }
    player.seek(seconds);
    player.take_pending_cut();
}

/// The render clock of a running job (no origin before frame 0 is next).
fn job_clock(job: &super::Job, run: &TakeRun) -> Clock {
    Clock {
        rate: Rate::of(&job.request),
        step_ns: job.request.step_ns(),
        origin_ns: (job.phase == Phase::Rendering).then_some(run.origin_ns).flatten(),
    }
}

/// `PostUpdate`, before the sequencer advances: while a job runs, every sequence player on the
/// external clock gets the job's timeline time ([`timeline_seconds`], 0 before frame 0), so its
/// tracks apply and its markers fire on the render clock, subframes included (ADR-1614,
/// ADR-1616 § 6). The time is not clamped to shots: a sequence is one continuous clock.
pub(crate) fn drive_job_sequences(
    time: Res<Time>,
    jobs: Option<Res<RenderOutJobs>>,
    mut players: Query<&mut SequencePlayer>,
) {
    let Some(jobs) = jobs else { return };
    let Some(job) = jobs.active.as_ref() else { return };
    let Some(run) = job.takes.as_ref() else { return };
    if run.sequence_players == 0 {
        return;
    }
    let seconds = timeline_seconds(
        job_clock(job, run),
        time.elapsed().as_nanos() as u64,
        job.delivered,
    );
    for mut player in &mut players {
        if player.clock == ClockSource::External {
            player.set_external_time(seconds);
        }
    }
}

/// The manifest's `job` block (ADR-1616): the plan, then per take its status, files, frame
/// hashes (the timeline's hashes of its frames on disk) and its sample range in the timeline WAV.
pub(crate) fn job_json(
    run: &TakeRun,
    results: &[TakeResult],
    request: &Request,
    hashes: &[u64],
    delivered: u64,
) -> Value {
    let mut block = run.plan.to_json();
    let takes: Vec<Value> = run
        .plan
        .takes
        .iter()
        .zip(&run.outs)
        .zip(results)
        .map(|((take, out), result)| {
            let mut entry = run.plan.take_json(take);
            let first = usize::try_from(take.start)
                .unwrap_or(usize::MAX)
                .min(hashes.len());
            let last = usize::try_from(take.start + result.written)
                .unwrap_or(usize::MAX)
                .min(hashes.len());
            entry["status"] = json!(result.status);
            entry["written"] = json!(result.written);
            entry["out"] = json!(request.shown(&out.request.out_dir));
            entry["files"] = json!(result.files.iter().map(|p| request.shown(p)).collect::<Vec<_>>());
            entry["frame_hashes"] = json!(
                hashes[first..last.max(first)]
                    .iter()
                    .map(|h| super::hex(*h))
                    .collect::<Vec<_>>()
            );
            entry["audio_samples"] = match &request.audio {
                Some(audio) => {
                    let offset = |frame: u64| {
                        let sim_ns = request.time_map.sim_ns(request.step_ns(), frame);
                        super::audio::file_offset(request, audio, frame, sim_ns)
                    };
                    json!({ "start": offset(take.start), "end": offset(take.start + result.written) })
                }
                None => Value::Null,
            };
            if let Some(error) = &result.error {
                entry["error"] = json!(request.scrub(error));
            }
            entry
        })
        .collect();
    block["takes"] = json!(takes);
    block["delivered"] = json!(delivered);
    block
}

/// A refused `render.out_job` with its lowercase code and a fix hint.
pub(crate) fn job_error(code: &'static str, message: impl Into<String>) -> RpcError {
    let hint = match code {
        "render_job_parse" | "render_job_invalid" => "docs/formats/render-job-ron.md lists every field",
        "render_job_version" => "the job file is from a newer engine; upgrade the engine",
        "render_job_unreadable" => {
            "paths are relative to the project root and confined like render.out_start `out`"
        }
        "render_job_bad_source" => {
            "fix the .shots.ron or .seq.ron the job names (render.sequence loads one to check it)"
        }
        "render_job_no_source" => "name the file the range needs in `shots` or `sequence`",
        "render_job_unknown_marker" => "the .seq.ron's markers are the names a Markers range or cut may use",
        "render_job_unknown_shot" => "render.sequence { action: \"query\" } lists the shot names",
        "render_job_bad_path" => {
            "tokens: {job}, {take}, {shot}, {frame}, {date}; parts are 1..=64 letters, digits, _, - or ."
        }
        "render_job_path_clash" => "put {take} in the path, or give one take its own path",
        "render_job_no_date" => "pass `date` (YYYY-MM-DD); regia render --job passes today's",
        "render_job_preroll_cap" => {
            "start the take nearer frame 0, add a take over the gap, or raise max_preroll_frames"
        }
        "render_job_unbound" => "scene.query lists entity names and guids; fix the job's `bindings` table",
        "render_job_bad_package" => {
            "`regia viewer verify` checks a package folder; book/agents/viewer-pack.md"
        }
        "render_job_build_refused" => {
            "a build must be this package's product and still sold; configurator.build writes one"
        }
        "render_job_pick_refused" => {
            "options() lists what is sold after each pick; reorder the picks or set `adjust: true`"
        }
        "render_job_outside_view_range" => {
            "the recipe's view_range is binding; viewer.view_simulate shows the limits, and `zoom` also depends on the output size"
        }
        _ => "book/agents/render-jobs.md",
    };
    RpcError::new(protocol::INVALID_PARAMS, code, message, hint)
}

fn put<T: serde::Serialize>(map: &mut serde_json::Map<String, Value>, key: &str, value: Option<T>) {
    if let Some(value) = value {
        map.insert(key.into(), json!(value));
    }
}

/// The `render.out_start` parameters of a job's output settings (only what the job sets, so the
/// render-out defaults apply to the rest and the one parser checks every value).
pub(crate) fn output_params(output: &OutputSettings) -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    put(&mut map, "width", output.width);
    put(&mut map, "height", output.height);
    put(&mut map, "fps", output.fps);
    put(&mut map, "fps_den", output.fps_den);
    put(&mut map, "supersample", output.supersample);
    put(&mut map, "format", output.format.as_ref());
    put(&mut map, "hud", output.hud);
    put(&mut map, "layout", output.layout.as_ref());
    put(&mut map, "calibration", output.calibration.as_ref());
    put(&mut map, "warmup_frames", output.warmup_frames);
    put(&mut map, "settle_frames", output.settle_frames);
    put(&mut map, "subframes", output.subframes);
    put(&mut map, "shutter_angle", output.shutter_angle);
    put(&mut map, "bit_depth", output.bit_depth);
    put(&mut map, "png_compression", output.png_compression.as_ref());
    put(&mut map, "writers", output.writers);
    put(&mut map, "aov_format", output.aov_format.as_ref());
    if let Some(encode) = &output.encode {
        map.insert("encode".into(), json!(encode));
    }
    if !output.aovs.is_empty() {
        map.insert("aovs".into(), json!(output.aovs));
    }
    if let Some(audio) = &output.audio {
        let mut entry = serde_json::Map::new();
        put(&mut entry, "sample_rate", audio.sample_rate);
        put(&mut entry, "format", audio.format.as_ref());
        put(&mut entry, "timeline", audio.timeline.as_ref());
        if !audio.stems.is_empty() {
            entry.insert("stems".into(), json!(audio.stems));
        }
        map.insert("audio".into(), Value::Object(entry));
    }
    if !output.time_map.is_empty() {
        let keys: Vec<Value> = output
            .time_map
            .iter()
            .map(|key| {
                let mut entry = serde_json::Map::new();
                entry.insert("frame".into(), json!(key.frame));
                entry.insert("scale".into(), json!(key.scale));
                put(&mut entry, "ease", key.ease.as_ref());
                Value::Object(entry)
            })
            .collect();
        map.insert("time_map".into(), Value::Array(keys));
    }
    map
}

/// The timeline request of a resolved job: its settings, `out`, the job name, the timeline length
/// and its cut frames.
pub(crate) fn timeline_request(job: &RenderJob, plan: &TakePlan) -> Result<Request, RpcError> {
    let mut params = output_params(&job.output);
    params.insert("out".into(), json!(job.out));
    params.insert("name".into(), json!(job.job_name()));
    params.insert("frames".into(), json!(plan.frames));
    params.insert("cuts".into(), json!(plan.cuts));
    Request::parse(&Value::Object(params))
}

/// `render.out_job { file | job, date?, dry_run? }`: reads and resolves a `.render_job.ron`,
/// answers its plan (`dry_run`) or starts it (ADR-1616).
pub(crate) fn out_job(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = super::out_root(world)
        .ok_or_else(|| RpcError::internal("no project root to resolve the job's paths against"))?;
    let extra_roots = world
        .get_resource::<crate::safety::AgentPermissions>()
        .map(|p| p.config.output_roots.clone())
        .unwrap_or_default();
    let read = |key: &str, path: &str| -> Result<String, RpcError> {
        let resolved =
            confine_out_dir(&root, &extra_roots, std::path::Path::new(path)).map_err(|mut e| {
                e.message = e.message.replace("`out`", &format!("`{key}`"));
                e
            })?;
        std::fs::read_to_string(&resolved).map_err(|e| {
            job_error(
                "render_job_unreadable",
                format!("cannot read `{key}` {path}: {e}"),
            )
        })
    };
    let text = match (
        crate::params::opt_str(params, "file")?,
        crate::params::opt_str(params, "job")?,
    ) {
        (Some(file), None) => read("file", file)?,
        (None, Some(text)) => text.to_string(),
        _ => {
            return Err(RpcError::invalid_params(
                "give `file` (a .render_job.ron path) or `job` (its RON text), one of them",
            ));
        }
    };
    let job = RenderJob::from_ron(&text).map_err(|e| job_error(e.code(), e.to_string()))?;
    let date = match params.get("date") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .filter(|date| is_valid_date(date))
                .ok_or_else(|| RpcError::invalid_params("`date` must be a YYYY-MM-DD string"))?,
        ),
    };
    let dry_run = match params.get("dry_run") {
        None | Some(Value::Null) => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`dry_run` must be a boolean"))?,
    };
    let shots = match &job.shots {
        Some(path) => Some(
            ShotSequence::from_ron(&read("shots", path)?)
                .map_err(|e| job_error("render_job_bad_source", format!("`shots` {path}: {e}")))?,
        ),
        None => None,
    };
    let sequence = match &job.sequence {
        Some(path) => Some(
            Sequence::from_ron(&read("sequence", path)?)
                .map_err(|e| job_error("render_job_bad_source", format!("`sequence` {path}: {e}")))?,
        ),
        None => None,
    };
    // The settings are checked (and the rate read) before anything is resolved against them.
    let mut probe = output_params(&job.output);
    probe.insert("out".into(), json!(job.out));
    probe.insert("name".into(), json!(job.job_name()));
    probe.insert("frames".into(), json!(1));
    let rate = Rate::of(&Request::parse(&Value::Object(probe))?);
    let mut sources = Sources {
        shots: shots.as_ref(),
        sequence: sequence.as_ref(),
        product_end: None,
    };
    let product_times = match &job.product {
        Some(product) => {
            let times = super::product::resolve_times(product, sources, rate)
                .map_err(|e| job_error(e.code, e.message))?;
            sources.product_end = times.last_frame();
            Some(times)
        }
        None => None,
    };
    let mut plan = plan(&job, sources, rate, date).map_err(|e| job_error(e.code, e.message))?;
    // ADR-2821: a take's folders come from its path template and its shot and take names.
    if let Some(take) = plan
        .takes
        .iter()
        .find(|t| t.dirs.iter().any(|d| crate::golden_tools::names_golden(d)))
    {
        return Err(crate::golden_tools::golden_path_denied(&take.dirs.join("/")));
    }
    // A product: the whole timeline is walked here, with no GPU (ADR-2757).
    let prepared = match (&job.product, &product_times) {
        (Some(product), Some(times)) => {
            let probe = timeline_request(&job, &plan)?;
            let resolve = |key: &str, path: &str| {
                confine_out_dir(&root, &extra_roots, std::path::Path::new(path)).map_err(|mut e| {
                    e.message = e.message.replace("`out`", &format!("`{key}`"));
                    e
                })
            };
            let timeline = super::product::Timeline {
                shots: shots.as_ref(),
                frames: plan.frames,
                rate,
                size: [probe.width, probe.height],
            };
            let prepared = super::product::prepare(product, times, timeline, &resolve, &mut plan.warnings)?;
            plan.product = prepared.plan_json();
            Some(prepared)
        }
        _ => None,
    };
    // A binding table: the job plays its sequence itself (ADR-1880).
    let job_player = match (&sequence, job.bindings.is_empty()) {
        (Some(sequence), false) => {
            let library = SequenceLibrary::read_tree(
                sequence,
                |name| read("sequence", name),
                |message| job_error("render_job_bad_source", format!("`sequence`: {message}")),
            )?;
            let resolved = resolve_job_bindings(world, &job)?;
            plan.bindings = resolved.iter().map(|(t, _, e)| (t.clone(), *e)).collect();
            plan.unbound_targets = unbound_targets(sequence, &library, &job);
            if !plan.unbound_targets.is_empty() {
                plan.warnings.push(WARN_UNBOUND_TARGETS);
            }
            Some(JobPlayer {
                sequence: sequence.clone(),
                library,
                bindings: resolved.into_iter().map(|(t, b, _)| (t, b)).collect(),
            })
        }
        _ => None,
    };
    // With a sequence player on the external clock the job plays the tracks (ADR-1614). A product job plays
    // none: its timeline drives the product, and the world's players stay as they are.
    let players = if prepared.is_some() {
        0
    } else {
        external_players(world) + usize::from(job_player.is_some())
    };
    if players > 0 {
        plan.warnings.retain(|warning| *warning != WARN_SEQUENCE_TRACKS);
    }
    plan.sequence_players = players;
    let mut request = timeline_request(&job, &plan)?;
    if prepared.is_some() {
        // No history, no game UI: a product frame is whole by itself.
        request.warmup = 0;
        request.settle = 0;
        request.hud = false;
    }
    if dry_run {
        request.confine_to(&root, &extra_roots)?;
        return Ok(json!({
            "dry_run": true,
            "plan": plan.to_json(),
            "request": super::request_json(&request),
        }));
    }
    // A host that opened no GPU on purpose refuses before a product job opens its own device below
    // (ADR-5180); a job on the game's renderer is checked in `start_request`.
    if prepared.is_some() {
        crate::seeing::GpuDisabled::refuse(world)?;
    }
    // A running job is refused before a device is made for another one.
    if prepared.is_some() && super::job_running(world) {
        return Err(RpcError::new(
            protocol::INVALID_REQUEST,
            "F0018",
            "a render-out is already running",
            "render.out_progress shows it; render.out_cancel stops it",
        ));
    }
    let product = prepared.map(|prepared| prepared.into_run(&request)).transpose()?;
    super::start_request(
        world,
        request,
        Some(super::PendingTakes {
            plan,
            // A product job samples the shot list itself; the game's shot player stays as it is.
            shots: if product.is_some() { None } else { shots },
            player: job_player,
            product,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    impl TakeRun {
        /// Hands timeline frame `index` over, waiting while an output is full.
        fn deliver(&mut self, index: u64, data: FrameData, aovs: Vec<AovFile>) -> Result<(), String> {
            match self.hand_over(index, data, aovs, &|| false)? {
                None => Ok(()),
                Some(_) => Err("not handed over".into()),
            }
        }
    }
    use crate::render_job::{DEFAULT_PATH, Take};

    const R30: Rate = Rate { num: 30, den: 1 };
    /// The render clock's step at 30 fps.
    const STEP30: u64 = 33_333_333;

    /// Shots of 1.0, 0.5 and 2.0 s (boundaries at 1.0 and 1.5 s: frames 30 and 45 at 30 fps).
    fn shots() -> ShotSequence {
        let shot = |name: &str, duration: f32| {
            format!(
                "(name: \"{name}\", duration: {duration:?}, camera: (path: (keys: [(time: 0.0, position: (0.0, 0.0, 0.0))], look_at: Point((0.0, 0.0, -1.0)))))"
            )
        };
        let text = format!(
            "(version: 2, shots: [{}, {}, {}])",
            shot("wide", 1.0),
            shot("mid", 0.5),
            shot("close", 2.0)
        );
        ShotSequence::from_ron(&text).expect("shots")
    }

    fn sequence() -> Sequence {
        Sequence::from_ron(
            r#"(version: 1, markers: [(name: "boom", time: 0.5), (name: "end", time: 2.0), (name: "late", time: 9.0)])"#,
        )
        .expect("sequence")
    }

    fn job(takes: Vec<Take>) -> RenderJob {
        RenderJob {
            version: 1,
            name: "trailer".into(),
            shots: None,
            sequence: None,
            bindings: Default::default(),
            out: "renders".into(),
            path: DEFAULT_PATH.into(),
            output: Default::default(),
            takes,
            cuts: Vec::new(),
            max_preroll_frames: None,
            product: None,
        }
    }

    fn take(name: &str, range: TakeRange) -> Take {
        Take {
            name: name.into(),
            range,
            path: None,
        }
    }

    fn both<'a>(shots: &'a ShotSequence, sequence: &'a Sequence) -> Sources<'a> {
        Sources {
            shots: Some(shots),
            sequence: Some(sequence),
            product_end: None,
        }
    }

    #[test]
    fn a_time_maps_to_the_first_frame_at_or_after_it() {
        assert_eq!(frame_at(0.0, R30), 0);
        assert_eq!(frame_at(-3.0, R30), 0);
        assert_eq!(frame_at(f64::NAN, R30), 0);
        assert_eq!(frame_at(1.0, R30), 30, "frame 30 at 30 fps is exactly 1 s");
        assert_eq!(frame_seconds(30, R30), 1.0);
        assert_eq!(frame_at(0.01, R30), 1);
        let ntsc = Rate {
            num: 30_000,
            den: 1001,
        };
        for rate in [R30, ntsc, Rate { num: 24, den: 1 }] {
            for frame in [1_u64, 2, 29, 30, 31, 999, 123_456] {
                let t = frame_seconds(frame, rate);
                assert_eq!(frame_at(t, rate), frame, "{rate:?} frame {frame}");
                assert!(frame_seconds(frame - 1, rate) < t);
            }
        }
        assert_eq!(
            frame_at(1e30, R30),
            MAX_FRAMES + 1,
            "too far is refused later, not looped to"
        );
    }

    #[test]
    fn every_shot_cut_frame_is_the_first_frame_the_player_shows_the_shot_on() {
        let shots = shots();
        let rates = [
            R30,
            Rate { num: 60, den: 1 },
            Rate {
                num: 24_000,
                den: 1001,
            },
            Rate { num: 25, den: 1 },
            Rate { num: 7, den: 1 },
        ];
        for rate in rates {
            let cuts = shot_cut_frames(&shots, rate);
            assert_eq!(cuts.len(), 2);
            for (n, cut) in cuts.iter().enumerate() {
                let index = n + 1;
                assert_eq!(
                    shot_of(&shots, *cut, rate),
                    index,
                    "{rate:?}: frame {cut} shows shot {index}"
                );
                assert!(
                    shot_of(&shots, cut - 1, rate) < index,
                    "{rate:?}: the frame before is the earlier shot"
                );
            }
        }
        assert_eq!(shot_cut_frames(&shots, R30), vec![30, 45]);
        assert_eq!(
            shot_cut_frames(&shots, Rate { num: 7, den: 1 }),
            vec![7, 11],
            "1.5 s · 7 = 10.5"
        );
    }

    #[test]
    fn every_range_kind_resolves_half_open() {
        let (shots, sequence) = (shots(), sequence());
        let sources = both(&shots, &sequence);
        let cases = [
            (TakeRange::Whole, (0, 105)),
            (TakeRange::Frames { start: 3, end: 9 }, (3, 9)),
            (TakeRange::Seconds { start: 0.5, end: 1.0 }, (15, 30)),
            (
                TakeRange::Markers {
                    from: "boom".into(),
                    to: "end".into(),
                },
                (15, 60),
            ),
            (
                TakeRange::Shots {
                    first: "mid".into(),
                    last: "mid".into(),
                },
                (30, 45),
            ),
            (
                TakeRange::Shots {
                    first: "wide".into(),
                    last: "close".into(),
                },
                (0, 105),
            ),
        ];
        for (range, want) in cases {
            assert_eq!(resolve_range(&range, sources, R30, "t"), Ok(want), "{range:?}");
        }
        // Without a shot list, `Whole` is the sequence's length (its latest marker).
        let only_sequence = Sources {
            shots: None,
            sequence: Some(&sequence),
            product_end: None,
        };
        assert_eq!(
            resolve_range(&TakeRange::Whole, only_sequence, R30, "t"),
            Ok((0, 270))
        );
    }

    #[test]
    fn missing_sources_names_and_empty_ranges_are_refused_with_their_codes() {
        let (shots, sequence) = (shots(), sequence());
        let code = |range: TakeRange, sources: Sources<'_>| {
            resolve_range(&range, sources, R30, "t").map_err(|e| e.code)
        };
        let markers = |from: &str, to: &str| TakeRange::Markers {
            from: from.into(),
            to: to.into(),
        };
        let shot_range = |first: &str, last: &str| TakeRange::Shots {
            first: first.into(),
            last: last.into(),
        };
        let none = Sources::default();
        assert_eq!(code(TakeRange::Whole, none), Err("render_job_no_source"));
        assert_eq!(code(markers("boom", "end"), none), Err("render_job_no_source"));
        assert_eq!(
            code(shot_range("wide", "wide"), none),
            Err("render_job_no_source")
        );
        let sources = both(&shots, &sequence);
        assert_eq!(
            code(markers("boom", "nope"), sources),
            Err("render_job_unknown_marker")
        );
        assert_eq!(
            code(shot_range("nope", "wide"), sources),
            Err("render_job_unknown_shot")
        );
        assert_eq!(
            code(shot_range("close", "wide"), sources),
            Err("render_job_invalid")
        );
        assert_eq!(
            code(markers("end", "boom"), sources),
            Err("render_job_invalid"),
            "backward"
        );
        assert_eq!(
            code(
                TakeRange::Seconds {
                    start: 0.001,
                    end: 0.002
                },
                sources
            ),
            Err("render_job_invalid"),
            "both ends map to frame 1"
        );
        assert_eq!(
            code(
                TakeRange::Frames {
                    start: 0,
                    end: MAX_FRAMES + 1
                },
                sources
            ),
            Err("render_job_invalid")
        );
    }

    #[test]
    fn cut_frames_are_the_shot_boundaries_inside_the_range_plus_the_explicit_cuts() {
        let (shots, sequence) = (shots(), sequence());
        let mut job = job(vec![
            take(
                "opening",
                TakeRange::Shots {
                    first: "wide".into(),
                    last: "mid".into(),
                },
            ),
            take("tail", TakeRange::Frames { start: 40, end: 100 }),
        ]);
        job.cuts = vec![
            CutAt::Frame(10),
            CutAt::Seconds(2.5),
            CutAt::Marker("boom".into()),
            CutAt::Frame(0),
            CutAt::Frame(30),
            CutAt::Marker("late".into()),
        ];
        let plan = plan(&job, both(&shots, &sequence), R30, None).expect("plan");
        assert_eq!(plan.frames, 100);
        assert_eq!(
            plan.cuts,
            vec![10, 15, 30, 45, 75],
            "0, a duplicate and a cut past the end are dropped"
        );
        assert!(plan.warnings.contains(&WARN_CUT_PAST_END));
        assert!(plan.warnings.contains(&WARN_SEQUENCE_TRACKS));
        let opening = &plan.takes[0];
        assert_eq!((opening.start, opening.end), (0, 45));
        assert_eq!(
            opening.cuts,
            vec![10, 15, 30],
            "the boundary at 45 is the take's end, not a cut in it"
        );
        let segments: Vec<(u64, u64, Option<&str>)> = opening
            .segments
            .iter()
            .map(|s| (s.start, s.end, s.shot.as_deref()))
            .collect();
        assert_eq!(
            segments,
            vec![
                (0, 10, Some("wide")),
                (10, 15, Some("wide")),
                (15, 30, Some("wide")),
                (30, 45, Some("mid")),
            ]
        );
        let tail = &plan.takes[1];
        assert_eq!(tail.cuts, vec![45, 75]);
        assert_eq!(tail.first_shot.as_deref(), Some("mid"));
        let json = plan.take_json(tail);
        assert_eq!(json["cuts"], json!([5, 35]), "take-local");
        assert_eq!(json["cut_frames"], json!([45, 75]), "global");
        assert_eq!(json["cut_list"][1]["shot"], json!("close"));
        assert_eq!(json["cut_list"][1]["local_start"], json!(5));
        assert_eq!(json["start_s"], json!(frame_seconds(40, R30)));
        assert_eq!(plan.preroll, 0, "0..45 and 40..100 cover the timeline");
        assert_eq!(plan.written_frames(), 45 + 60, "the overlap counts in both takes");
        let whole = plan.to_json();
        assert_eq!(whole["frames"], json!(100));
        assert_eq!(whole["takes"].as_array().map(Vec::len), Some(2));
        assert_eq!(
            whole["warnings"],
            json!([WARN_SEQUENCE_TRACKS, WARN_CUT_PAST_END])
        );
    }

    #[test]
    fn a_cut_marker_without_a_sequence_is_refused() {
        let mut job = job(vec![take("t", TakeRange::Frames { start: 0, end: 4 })]);
        job.cuts = vec![CutAt::Marker("boom".into())];
        let error = plan(&job, Sources::default(), R30, None).expect_err("no sequence");
        assert_eq!(error.code, "render_job_no_source");
    }

    #[test]
    fn preroll_counts_frames_no_take_writes_and_the_cap_refuses_above_it() {
        let mut job = job(vec![
            take("late", TakeRange::Frames { start: 50, end: 60 }),
            take("mid", TakeRange::Frames { start: 20, end: 30 }),
            take("again", TakeRange::Frames { start: 25, end: 35 }),
        ]);
        let plan = plan(&job, Sources::default(), R30, None).expect("plan");
        assert_eq!(plan.frames, 60);
        assert_eq!(plan.preroll, 60 - 10 - 15, "0..20 and 35..50 are pre-roll");
        job.max_preroll_frames = Some(35);
        assert!(
            super::plan(&job, Sources::default(), R30, None).is_ok(),
            "at the cap"
        );
        job.max_preroll_frames = Some(34);
        let error = super::plan(&job, Sources::default(), R30, None).expect_err("over the cap");
        assert_eq!(error.code, "render_job_preroll_cap");
        assert!(error.message.contains("35 frames"), "{}", error.message);
    }

    #[test]
    fn path_tokens_expand_into_folders_and_a_stem() {
        let shots = shots();
        let mut job = job(vec![
            take("t1", TakeRange::Frames { start: 31, end: 40 }),
            Take {
                name: "t2".into(),
                range: TakeRange::Frames { start: 0, end: 5 },
                path: Some("flat_{take}".into()),
            },
        ]);
        job.path = "{date}/{job}/{take}/{shot}_{frame}".into();
        let sources = Sources {
            shots: Some(&shots),
            sequence: None,
            product_end: None,
        };
        let plan = plan(&job, sources, R30, Some("2026-10-28")).expect("plan");
        assert_eq!(plan.takes[0].dirs, vec!["2026-10-28", "trailer", "t1"]);
        assert_eq!(plan.takes[0].stem, "mid_000031");
        assert_eq!(plan.takes[0].dir_text(), "2026-10-28/trailer/t1");
        assert!(
            plan.takes[1].dirs.is_empty(),
            "a per-take template overrides the job's"
        );
        assert_eq!(plan.takes[1].stem, "flat_t2");
        assert_eq!(plan.takes[1].dir_text(), ".");
    }

    #[test]
    fn bad_templates_a_missing_date_and_clashing_takes_are_refused() {
        let shots = shots();
        let with_shots = Sources {
            shots: Some(&shots),
            sequence: None,
            product_end: None,
        };
        let code = |path: &str, sources: Sources<'_>, date: Option<&str>| {
            let mut one = job(vec![take("t", TakeRange::Frames { start: 0, end: 2 })]);
            one.path = path.into();
            plan(&one, sources, R30, date).map(|_| ()).map_err(|e| e.code)
        };
        for bad in [
            "{take}/{bogus}",
            "{take",
            "take}",
            "a//{take}",
            "../{take}",
            "/{take}",
            "{take}/",
        ] {
            assert_eq!(code(bad, with_shots, None), Err("render_job_bad_path"), "{bad}");
        }
        assert_eq!(
            code("{shot}", Sources::default(), None),
            Err("render_job_bad_path")
        );
        assert_eq!(code("{date}_{take}", with_shots, None), Err("render_job_no_date"));
        assert_eq!(code("{date}_{take}", with_shots, Some("2026-10-28")), Ok(()));
        let same = |name: &str, start: u64| Take {
            name: name.into(),
            range: TakeRange::Frames {
                start,
                end: start + 2,
            },
            path: Some("same/name".into()),
        };
        let clash = job(vec![same("a", 0), same("b", 2)]);
        let error = plan(&clash, Sources::default(), R30, None).expect_err("clash");
        assert_eq!(error.code, "render_job_path_clash");
        assert!(error.message.contains("`a` and `b`"), "{}", error.message);
    }

    #[test]
    fn the_player_time_follows_the_render_clock_and_stays_inside_the_frames_shot() {
        let shots = shots();
        let origin = 5_000_000_000_u64;
        let clock = Clock {
            rate: R30,
            step_ns: STEP30,
            origin_ns: Some(origin),
        };
        let before = Clock {
            origin_ns: None,
            ..clock
        };
        assert_eq!(
            shot_time(&shots, before, 9_000_000_000, 10),
            0.0,
            "before frame 0"
        );
        assert_eq!(
            shot_time(&shots, clock, 4_000_000_000, 0),
            0.0,
            "never before the start"
        );
        for frame in [0_u64, 7, 29, 30, 44, 45, 104] {
            assert_eq!(
                shot_time(&shots, clock, origin + frame * STEP30, frame),
                frame_seconds(frame, R30),
                "frame {frame}'s own time, by the same conversion as the plan"
            );
        }
        // Frame 29 is the last of `wide`; a late subframe past 1.0 s stays in `wide`.
        let late = shot_time(&shots, clock, origin + 29 * STEP30 + 40_000_000, 29);
        assert!(late < 1.0, "{late}");
        assert_eq!(shots.locate(late).map(|(i, _)| i), Some(0));
        // Frame 30 is the first of `mid`; an early subframe before 1.0 s stays in `mid`.
        let early = shot_time(&shots, clock, origin + 30 * STEP30 - 10_000_000, 30);
        assert_eq!(early, 1.0);
        assert_eq!(shots.locate(early).map(|(i, _)| i), Some(1));
        // A subframe inside its shot keeps its offset.
        let mid = shot_time(&shots, clock, origin + 7 * STEP30 + 5_000_000, 7);
        assert!((mid - (frame_seconds(7, R30) + 0.005)).abs() < 1e-12, "{mid}");
        // The last shot holds its end.
        assert_eq!(shot_time(&shots, clock, origin + 9_000_000_000, 104), 3.5);
    }

    #[test]
    fn uncovered_merges_overlapping_and_touching_ranges() {
        assert_eq!(uncovered(10, &[]), 10);
        assert_eq!(uncovered(10, &[(0, 10)]), 0);
        assert_eq!(uncovered(10, &[(2, 4), (3, 6), (6, 7)]), 5);
        assert_eq!(uncovered(10, &[(8, 10), (0, 1)]), 7);
    }

    #[test]
    fn a_take_run_writes_each_frame_into_every_take_holding_it_numbered_from_zero() {
        let dir = regia_test_utils::unique_temp_dir("render-job-takes");
        let job = job(vec![
            take("a", TakeRange::Frames { start: 1, end: 3 }),
            take("b", TakeRange::Frames { start: 2, end: 4 }),
        ]);
        let plan = plan(&job, Sources::default(), R30, None).expect("plan");
        let timeline = Request::parse(&json!({
            "out": dir.display().to_string(), "frames": plan.frames, "width": 2, "height": 1, "writers": 1,
        }))
        .expect("request");
        let mut run = TakeRun::new(plan, &timeline, None);
        for index in 0..4_u64 {
            let pixel = u8::try_from(index).expect("small");
            run.deliver(index, FrameData::Rgba8(vec![pixel; 8]), Vec::new())
                .expect("written");
            assert_eq!(
                run.writing(index).len(),
                usize::from(index == 2) + 1 - usize::from(index == 0)
            );
        }
        let results = run.finish(4, "done");
        assert_eq!(
            results.iter().map(|r| r.status).collect::<Vec<_>>(),
            vec!["done", "done"]
        );
        assert_eq!(results.iter().map(|r| r.written).collect::<Vec<_>>(), vec![2, 2]);
        let read = |path: &std::path::Path| {
            let decoder =
                png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).expect("file")));
            let mut reader = decoder.read_info().expect("png");
            let mut buf = vec![0; reader.output_buffer_size().expect("size")];
            reader.next_frame(&mut buf).expect("frame");
            buf[0]
        };
        assert!(!dir.join("a").join("a_000002.png").exists(), "a holds two frames");
        assert_eq!(
            read(&dir.join("a").join("a_000000.png")),
            1,
            "take-local 0 is timeline frame 1"
        );
        assert_eq!(read(&dir.join("a").join("a_000001.png")), 2);
        assert_eq!(
            read(&dir.join("b").join("b_000000.png")),
            2,
            "the shared frame went to both"
        );
        assert_eq!(read(&dir.join("b").join("b_000001.png")), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cancelled_run_reports_takes_it_did_not_reach_and_the_one_it_stopped_in() {
        let dir = regia_test_utils::unique_temp_dir("render-job-cancel");
        let job = job(vec![
            take("first", TakeRange::Frames { start: 0, end: 2 }),
            take("cut_short", TakeRange::Frames { start: 2, end: 6 }),
            take("never", TakeRange::Frames { start: 6, end: 8 }),
        ]);
        let plan = plan(&job, Sources::default(), R30, None).expect("plan");
        let timeline = Request::parse(&json!({
            "out": dir.display().to_string(), "frames": plan.frames, "width": 1, "height": 1, "writers": 1,
        }))
        .expect("request");
        let mut run = TakeRun::new(plan, &timeline, None);
        for index in 0..3_u64 {
            run.deliver(index, FrameData::Rgba8(vec![9; 4]), Vec::new())
                .expect("written");
        }
        let results = run.finish(3, "cancelled");
        let statuses: Vec<_> = results.iter().map(|r| (r.status, r.written)).collect();
        assert_eq!(statuses, vec![("done", 2), ("cancelled", 1), ("not_reached", 0)]);
        assert!(!dir.join("never").exists(), "an unreached take opens nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A world whose project root is a fresh directory holding the test shot list.
    fn project_world(tag: &str) -> (World, std::path::PathBuf) {
        let root = regia_test_utils::unique_temp_dir(tag);
        std::fs::create_dir_all(root.join("cine")).expect("cine dir");
        std::fs::write(
            root.join("cine").join("t.shots.ron"),
            shots().to_ron().expect("ron"),
        )
        .expect("shots");
        std::fs::write(root.join("cine").join("bad.shots.ron"), "(version: 2, shots: [])")
            .expect("bad shots");
        let mut world = World::new();
        world.insert_resource(crate::AgentSettings {
            project_dir: Some(root.clone()),
        });
        (world, root)
    }

    fn code_of(error: &RpcError) -> Option<&str> {
        error.data.as_ref().and_then(|d| d["code"].as_str())
    }

    #[test]
    fn a_dry_run_answers_the_plan_and_starts_nothing() {
        let (mut world, root) = project_world("render-job-dry");
        let text = r#"(version: 1, name: "t", shots: Some("cine/t.shots.ron"), out: "renders",
            path: "{date}/{take}", output: (fps: Some(30), width: Some(64), height: Some(36)),
            takes: [(name: "mid", range: Shots(first: "mid", last: "mid"))])"#;
        std::fs::write(root.join("cine").join("t.render_job.ron"), text).expect("job file");
        let answer = out_job(
            &mut world,
            &json!({ "file": "cine/t.render_job.ron", "date": "2026-10-28", "dry_run": true }),
        )
        .expect("plan");
        assert_eq!(answer["dry_run"], json!(true));
        let plan = &answer["plan"];
        assert_eq!(plan["frames"], json!(45));
        assert_eq!(plan["preroll_frames"], json!(30), "0..30 is pre-roll");
        assert_eq!(plan["cuts"], json!([30]), "the boundary at 45 is the end");
        assert_eq!(plan["takes"][0]["start_frame"], json!(30));
        assert_eq!(plan["takes"][0]["dir"], json!("2026-10-28"));
        assert_eq!(plan["takes"][0]["stem"], json!("mid"));
        assert_eq!(answer["request"]["frames"], json!(45));
        assert_eq!(answer["request"]["cuts"], json!([30]));
        assert_eq!(answer["request"]["width"], json!(64));
        assert!(
            world
                .get_resource::<RenderOutJobs>()
                .is_none_or(|jobs| jobs.active.is_none()),
            "nothing runs"
        );
        // The same job inline, without a date for `{date}`.
        let error = out_job(&mut world, &json!({ "job": text, "dry_run": true })).expect_err("no date");
        assert_eq!(code_of(&error), Some("render_job_no_date"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn out_job_refuses_with_the_documented_codes() {
        let (mut world, root) = project_world("render-job-codes");
        let job = |body: &str| format!("(version: 1, out: \"renders\", {body})");
        let cases = [
            (json!({}), "F0002"),
            (
                json!({ "job": job("takes: [(name: \"a\", range: Whole)]"), "file": "x" }),
                "F0002",
            ),
            (
                json!({ "file": "cine/missing.render_job.ron" }),
                "render_job_unreadable",
            ),
            (json!({ "job": "(version: 1," }), "render_job_parse"),
            (
                json!({ "job": "(version: 7, out: \"r\", takes: [])" }),
                "render_job_version",
            ),
            (json!({ "job": job("takes: []") }), "render_job_invalid"),
            (
                json!({ "job": job("shots: Some(\"cine/nope.shots.ron\"), takes: [(name: \"a\", range: Whole)]") }),
                "render_job_unreadable",
            ),
            (
                json!({ "job": job("shots: Some(\"cine/bad.shots.ron\"), takes: [(name: \"a\", range: Whole)]") }),
                "render_job_bad_source",
            ),
            (
                json!({ "job": job("takes: [(name: \"a\", range: Whole)]") }),
                "render_job_no_source",
            ),
            (
                json!({ "job": job("shots: Some(\"cine/t.shots.ron\"), takes: [(name: \"a\", range: Shots(first: \"zoom\", last: \"wide\"))]") }),
                "render_job_unknown_shot",
            ),
            (
                json!({ "job": job("takes: [(name: \"a\", range: Frames(start: 0, end: 2))], output: (fps: Some(0))") }),
                "F0002",
            ),
            (
                json!({ "job": job("takes: [(name: \"a\", range: Frames(start: 0, end: 2))]"), "date": "yesterday" }),
                "F0002",
            ),
            (
                json!({ "job": job("takes: [(name: \"a\", range: Frames(start: 0, end: 2))]"), "dry_run": "yes" }),
                "F0002",
            ),
            (
                json!({ "job": "(version: 1, out: \"../outside\", takes: [(name: \"a\", range: Frames(start: 0, end: 2))])", "dry_run": true }),
                "F0002",
            ),
        ];
        for (params, want) in cases {
            let error = out_job(&mut world, &params).expect_err(&params.to_string());
            assert_eq!(code_of(&error), Some(want), "{params}: {error:?}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn starting_without_a_renderer_is_refused_before_the_shot_player_is_touched() {
        let (mut world, root) = project_world("render-job-norenderer");
        let text = r#"(version: 1, shots: Some("cine/t.shots.ron"), out: "renders", takes: [(name: "a", range: Whole)])"#;
        let error = out_job(&mut world, &json!({ "job": text })).expect_err("no renderer");
        assert_eq!(code_of(&error), Some("F0011"));
        assert!(
            world.get_resource::<ShotPlayer>().is_none(),
            "the player was not loaded"
        );
        assert!(!root.join("renders").exists(), "no output was created");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ADR-5180: a product job draws on a device of its own (`PreparedProduct::into_run`), so a host
    /// that opened no GPU on purpose (`GpuDisabled`, the headless editor's `--no-gpu`) refuses it
    /// with its code before that device is opened, starts no job and writes nothing. Its dry run
    /// opens no device and is still answered.
    #[test]
    fn a_host_without_a_gpu_refuses_a_product_job_before_its_device() {
        use regia_viewer_pack::cook::{self, CookOptions};

        let (mut world, root) = project_world("render-job-product-nogpu");
        let set = regia_viewer_pack::standin::write(&root.join("in")).expect("stand-ins");
        let (recipe, graph) = set
            .recipes
            .iter()
            .zip(&set.option_graphs)
            .find(|(recipe, _)| {
                recipe
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.contains("marquise"))
            })
            .expect("a marquise stand-in");
        let options = CookOptions {
            option_graph: Some(graph.clone()),
            materials: Some(set.library.clone()),
            ..CookOptions::default()
        };
        cook::pack(&set.kit_dir, recipe, &root.join("products").join("a"), &options).expect("cooks");
        let text = r#"(version: 3, name: "nogpu", out: "out",
            output: (width: Some(32), height: Some(32), fps: Some(10), writers: Some(1)),
            takes: [(name: "a", range: Frames(start: 0, end: 2))],
            product: Some((package: "products/a")))"#;
        world.insert_resource(crate::seeing::GpuDisabled {
            code: "editor_headless".into(),
            reason: "the headless editor has no GPU adapter (started with --no-gpu)".into(),
            hint: "start it without --no-gpu".into(),
        });
        let opened = || super::super::product::RIGS_OPENED.with(std::cell::Cell::get);
        let before = opened();
        let plan = out_job(&mut world, &json!({ "job": text, "dry_run": true })).expect("a dry run");
        assert!(plan["plan"]["product"].is_object(), "{plan}");
        let error = out_job(&mut world, &json!({ "job": text })).expect_err("refused");
        assert_eq!(error.code, protocol::UNSUPPORTED, "{error:?}");
        assert_eq!(code_of(&error), Some("editor_headless"), "{error:?}");
        assert_eq!(opened(), before, "no product device was opened");
        assert!(
            world.get_resource::<regia_render::RenderDevice>().is_none(),
            "no device"
        );
        assert!(
            world
                .get_resource::<RenderOutJobs>()
                .is_none_or(|jobs| jobs.active.is_none()),
            "no job started"
        );
        assert!(!root.join("out").exists(), "nothing was written");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn output_settings_become_exactly_the_render_out_params_they_set() {
        let mut output = OutputSettings::default();
        assert!(output_params(&output).is_empty(), "nothing set, nothing sent");
        output.width = Some(640);
        output.format = Some("raw".into());
        output.aovs = vec!["depth".into()];
        output.audio = Some(crate::render_job::AudioSettings {
            stems: vec!["music".into()],
            ..Default::default()
        });
        output.time_map = vec![crate::render_job::TimeMapKey {
            frame: 4,
            scale: 0.5,
            ease: None,
        }];
        let params = Value::Object(output_params(&output));
        assert_eq!(
            params,
            json!({
                "width": 640, "format": "raw", "aovs": ["depth"],
                "audio": { "stems": ["music"] },
                "time_map": [{ "frame": 4, "scale": 0.5 }],
            })
        );
    }

    #[test]
    fn the_shot_drive_loads_the_job_list_and_puts_the_player_back() {
        let mut world = World::new();
        let mut before = ShotPlayer::default();
        let own = ShotSequence::from_ron(
            r#"(version: 2, shots: [(name: "own", duration: 9.0, camera: (path: (keys: [(time: 0.0, position: (0.0, 0.0, 0.0))])))])"#,
        )
        .expect("own");
        before.load(own.clone());
        before.set_looping(true);
        before.seek(2.5);
        before.play();
        world.insert_resource(before);
        let job = job(vec![take("a", TakeRange::Whole)]);
        let list = shots();
        let sources = Sources {
            shots: Some(&list),
            sequence: None,
            product_end: None,
        };
        let plan = plan(&job, sources, R30, None).expect("plan");
        let timeline =
            Request::parse(&json!({ "out": "/tmp/never", "frames": plan.frames })).expect("request");
        let mut run = TakeRun::new(plan, &timeline, Some(shots()));
        begin_shots(&mut world, &mut run);
        {
            let mut player = world.resource_mut::<ShotPlayer>();
            assert_eq!(player.sequence(), Some(&shots()));
            assert!(!player.is_playing() && !player.is_looping());
            assert!(!player.take_pending_cut(), "the job cuts, not the player");
        }
        end_shots(&mut world, &mut run);
        let player = world.resource::<ShotPlayer>();
        assert_eq!(player.sequence(), Some(&own));
        assert_eq!(player.time(), 2.5);
        assert!(player.is_playing() && player.is_looping());
    }

    #[test]
    fn the_timeline_time_follows_the_render_clock_without_shot_clamping() {
        let origin = 2_000_000_000_u64;
        let clock = Clock {
            rate: R30,
            step_ns: STEP30,
            origin_ns: Some(origin),
        };
        let before = Clock {
            origin_ns: None,
            ..clock
        };
        assert_eq!(timeline_seconds(before, 9_000_000_000, 4), 0.0, "before frame 0");
        assert_eq!(timeline_seconds(clock, 1_000_000_000, 0), 0.0, "never below 0");
        for frame in [0_u64, 1, 29, 30, 31, 300] {
            assert_eq!(
                timeline_seconds(clock, origin + frame * STEP30, frame),
                frame_seconds(frame, R30),
                "frame {frame}"
            );
        }
        // A late subframe of the last `wide` frame crosses 1.0 s: the sequence clock does, the
        // shot clock stays in `wide`.
        let elapsed = origin + 29 * STEP30 + 40_000_000;
        assert!(timeline_seconds(clock, elapsed, 29) > 1.0);
        assert!(shot_time(&shots(), clock, elapsed, 29) < 1.0);
    }

    #[test]
    fn external_clock_players_restart_at_zero_and_gameplay_players_are_left_alone() {
        let sequence = Sequence::from_ron(
            r#"(version: 1, markers: [(name: "zero", time: 0.0), (name: "late", time: 2.0)])"#,
        )
        .expect("sequence");
        let mut world = World::new();
        let mut external = SequencePlayer::new(sequence.clone());
        external.clock = ClockSource::External;
        let external = world.spawn(external).id();
        let mut gameplay = SequencePlayer::new(sequence);
        gameplay.play();
        let gameplay = world.spawn(gameplay).id();
        assert_eq!(external_players(&mut world), 1);
        let job = job(vec![take("a", TakeRange::Frames { start: 0, end: 2 })]);
        let plan = plan(&job, Sources::default(), R30, None).expect("plan");
        let timeline =
            Request::parse(&json!({ "out": "/tmp/never", "frames": plan.frames })).expect("request");
        let mut run = TakeRun::new(plan, &timeline, None);
        begin_sequences(&mut world, &mut run);
        assert_eq!(run.sequence_players, 1);
        let mut player = world.get_mut::<SequencePlayer>(external).expect("external");
        assert_eq!(player.status().name(), "playing");
        assert_eq!(player.time(), 0.0);
        player.set_external_time(1.0);
        assert_eq!(player.time(), 1.0, "the job's time moves it");
        let fired: Vec<String> = player.take_fired().into_iter().map(|m| m.name).collect();
        assert_eq!(fired, ["zero"], "markers at 0 fire on the first forward step");
        let other = world.get::<SequencePlayer>(gameplay).expect("gameplay");
        assert_eq!(other.clock, ClockSource::Gameplay);
    }
}
