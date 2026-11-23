# Render jobs: render-out driven by a sequence

A render job describes a whole render in one file: the shot list to play, the takes to write,
where each take goes, the cuts and the output settings. `render.out_job` runs it in a running app,
and `regia render --job` runs it from the command line (a farm, a script). It builds on
[render-out](render-out.md): every `render.out_start` setting works the same way, and so do
progress, cancel and the manifest (ADR-1616).

## The model: one timeline from time 0

A job renders one **timeline** of frames `0..end`, where frame `f` is at sequence time
`f · fps_den / fps` seconds, and `end` is the latest take end. Each frame is rendered the way a
plain render-out of `0..end` renders it. It is then written into every take whose range holds it,
or into none. So:

- **A take is byte-identical to the same frames of a full render.** A take that starts at 40 s
  does not cut or reset at its first frame. Everything before it was simulated and rendered,
  so the world, the temporal history and the exposure are what a full render has there.
- **Pre-roll costs GPU time.** Frames that no take holds are rendered and not written. The plan
  reports them as `preroll_frames`, and `max_preroll_frames` in the job refuses a plan above the
  cap (`render_job_preroll_cap`). Put early takes in the same job, or start takes near 0, to keep
  the cost down.
- **Takes may overlap.** A shared frame is written to each of them.
- **One render-out at a time** still holds (`F0018`).

## The job file (`.render_job.ron`)

```ron,ignore
(
    version: 1,
    name: "trailer",
    shots: Some("cinematics/trailer.shots.ron"),
    sequence: Some("cinematics/trailer.seq.ron"),
    out: "renders/trailer",
    path: "{date}/{take}/{job}_{take}",
    output: (width: Some(1920), height: Some(1080), fps: Some(24), supersample: Some(2), settle_frames: Some(16), warmup_frames: Some(16)),
    takes: [
        (name: "opening", range: Shots(first: "wide", last: "close")),
        (name: "blast", range: Markers(from: "boom", to: "dust_settled")),
        (name: "tail", range: Seconds(start: 42.0, end: 48.5)),
        (name: "frames", range: Frames(start: 120, end: 240), path: Some("{take}/raw_{frame}")),
        (name: "all", range: Whole),
    ],
    cuts: [Marker("flash"), Seconds(30.0)],
    max_preroll_frames: Some(2400),
)
```

The full field list is in `docs/formats/render-job-ron.md`. Paths (`shots`, `sequence`, `out`,
and `file` in the call) are relative to the project root and confined like `render.out_start`
`out`.

### Take ranges

Every range is half-open: it holds its start and not its end. A time maps to the first frame at
or after it.

| Range | Frames |
|---|---|
| `Whole` | The shot list's length (else the sequence's: its latest key or marker). |
| `Frames(start, end)` | Timeline frames `start..end`. |
| `Seconds(start, end)` | The frames from `start` s to `end` s. |
| `Markers(from, to)` | From `.seq.ron` marker `from` to marker `to`. |
| `Shots(first, last)` | From the start of shot `first` to the end of shot `last` (`first` may equal `last`). |

### Output paths

`path` (job-wide; a take's `path` overrides it) is a `/`-separated template. The last part is the
file stem; the parts before it are folders under `out`. Tokens:

| Token | Value |
|---|---|
| `{job}` | The job name (`job` when empty). |
| `{take}` | The take name. |
| `{shot}` | The shot at the take's first frame (needs `shots`). |
| `{frame}` | The take's first timeline frame, 6 digits. |
| `{date}` | The `date` the caller gave (YYYY-MM-DD). The engine reads no clock; `regia render --job` passes today's UTC date. |

Every expanded part must be 1..=64 letters, digits, `_`, `-` or `.` and must not start with `.`.
Files inside a take are numbered from 0 (`<stem>_000000.png`), and the manifest maps them to
timeline frames. The default `path` is `{take}/{take}`.

## Cuts and the cut list

The job's cut frames are every shot boundary of the shot list (the first frame that shows the next
shot) plus the job's `cuts` (`Frame(n)`, `Seconds(t)`, `Marker(name)`). Frame 0 is always the
job's own opening cut. Each cut is a metered render-out cut (history restart and exposure, as for
`render.out_start` `cuts`). A cut at or past the end is dropped with the warning `cut_past_end`.

Each take reports:

- `cut_frames`: the cuts inside it, as timeline frames;
- `cuts`: the same cuts as take-local frames (the frame numbers of its files);
- `cut_list`: the take split at its cuts, each segment with `shot` (the shot at its first frame),
  `start_frame` / `end_frame` (timeline) and `local_start` / `local_end` (take-local). This is the
  edit list for the take.

## The shot list drives the camera

With `shots`, the job loads that shot list into the shot player ([camera paths](camera-paths.md))
and holds it at 0 through arming, warm-up and settle frames. From frame 0 on it seeks the player
every app frame to the render clock's time, so shutter subframes move the camera (motion blur). A
frame's subframes are clamped into that frame's shot, so a shutter never straddles a cut. The
player's own cut is dropped, because the job cuts at the boundary itself. When the job ends, the
player gets back the sequence, time, loop and play state it had.

A `.seq.ron` named in `sequence` resolves `Markers` ranges and `Marker` cuts. Its tracks are played
by [sequence players](sequence-player.md) on the **external clock**: at the start the job restarts
every `SequencePlayer` whose `clock` is `External` at 0, then writes the timeline time into each one
every app frame. That time is unclamped and includes shutter subframes. So their tracks apply and
their markers fire (`MarkerReached`) on the render clock, and a take alone still matches a full
render. Set those players up (bindings included) before the job, for example with
`sequence.player { action: "load", clock: "external", ... }`. Give the shot list to the job or to a
sequence player, not to both. The plan reports `sequence_players`. When there are none, the warning
`sequence_tracks_not_applied` says the tracks will not move.

## A product instead of the world

A job with a `product` block (v3) draws a product package with the viewer's raster renderer, and its frames
then go through the same takes, writers and manifest. It takes a build, a camera path and timed picks, and needs no
game renderer. See [capture hooks](capture-hooks.md).

## `render.out_job`

```json
{ "method": "render.out_job", "params": { "file": "cinematics/trailer.render_job.ron", "date": "2026-10-28", "dry_run": true } }
```

| Param | Meaning |
|---|---|
| `file` | The job file, relative to the project root. Give `file` or `job`. |
| `job` | The job file's RON text. |
| `date` | `YYYY-MM-DD` for `{date}`. |
| `dry_run` | Answer the plan and render nothing. |

The answer of a started job is the `render.out_start` answer plus `plan`. With `dry_run` it is
`{ dry_run: true, plan, request }`. The plan has `job`, `frames`, `fps`, `fps_den`,
`preroll_frames`, `written_frames`, `cuts`, `warnings`, `product` (`null` without one) and, per take, `name`, `start_frame`,
`end_frame`, `frames`, `start_s`, `end_s`, `dir`, `stem`, `first_shot`, `cut_frames`, `cuts` and
`cut_list`.

`render.out_progress` adds `takes: { writing, preroll_frames, takes }` while the job runs (the
takes being written at the current frame), and the manifest's `takes` once it ends.
`render.out_cancel` stops the job. Takes it finished stay `done`, the take it stopped in keeps its
frames up to that point, and later takes are `not_reached`.

### The manifest

The job writes `<out>/<job>_render_out.json` (`regia.render_out/2`). `frame_hashes` covers the
whole timeline, `files.frames` is empty (the frames are per take), and the `job` block holds the
plan plus, per take:

- `status`: `done`, `cancelled`, `failed` or `not_reached`;
- `written`, `out`, `files`;
- `frame_hashes`: the timeline hashes of its frames, so they compare directly with a full render;
- `audio_samples`: `{ start, end }`, its range in the timeline WAV (`null` without audio).

Audio is one WAV of the whole timeline (`<job>_audio.wav`), and each take names its sample range
in it.

## `regia render --job`

```sh
regia render --job cinematics/trailer.render_job.ron --project . [--date 2026-10-28] [--dry-run] [--job-timeout-s 0]
```

The command checks the file, connects to the running app of `--project` (start it headless on a
farm), runs the job, prints the manifest path and exits with:

| Exit | Meaning |
|---|---|
| 0 | Done (with `--dry-run`: the plan was printed). |
| 1 | No running app, or a call failed. |
| 2 | The job file or the job was refused (bad file, newer version, bad date, a `render_job_*` error, a job already running). |
| 3 | The job failed. |
| 4 | The job was cancelled (`--job-timeout-s` cancels it after that many seconds). |

## The binding table: a job that plays its own sequence (v2, ADR-1880)

A `.render_job.ron` v2 may carry a `bindings` table from the `sequence`'s target names to entities:

```ron
(
    version: 2,
    shots: Some("cinematics/trailer.shots.ron"),
    sequence: Some("cinematics/trailer.seq.ron"),
    bindings: { "hero": Name("Hero"), "lamp": Guid("00000000-0000-0000-0000-00000000002a") },
    out: "renders/trailer",
    takes: [(name: "all", range: Whole)],
)
```

With a table, the job plays the sequence itself. Before anything starts it reads the sequence's
sub-sequences from their project paths (the names its `Sub` sections use), resolves every binding
(`Name` binds the lowest entity with that name, by its guid when it has one; `Guid` the entity with that
guid; neither found is `render_job_unbound`), then creates an external-clock sequence player with those
bindings for the run and removes it at the end, and puts back the sequence library entries it added. The
plan counts it in `sequence_players` and lists the table as `bindings: [{ target, entity }]`; targets of the
sequence and its sub-sequences the table leaves out do not stop the job, but they do not animate: the plan
lists them in `unbound_targets` and warns `sequence_targets_unbound`. A dry run answers the same plan and
creates nothing. The
sequence's clip tracks, audio tracks and sub-sequences play on the render clock like its property tracks;
the clips and sounds they name come from the app's clip library and `SoundAssets`. Without a table the job
drives the external-clock players the host created, as before. A v1 job loads unchanged
(`regia upgrade-project` re-saves it at v2).

A sequence's video sections (`.seq.ron` v4) play frame-exact in a job: each output frame blocks until its exact
source picture is decoded, with no drops, and the source's own sound joins the timeline WAV (see video sections in
[Sequence players](sequence-player.md); the app needs `regia_media::MediaPlugin`).

## Error codes

| Code | Meaning |
|---|---|
| `render_job_parse` | The text is not valid `.render_job.ron`. |
| `render_job_version` | The file's version is newer than this engine reads. |
| `render_job_invalid` | No takes, a duplicate or invalid take name, an empty or reversed range, or more than 1 000 000 frames. |
| `render_job_unreadable` | The job file, its `.shots.ron` or its `.seq.ron` cannot be read. |
| `render_job_bad_source` | That `.shots.ron` or `.seq.ron` does not parse or validate. |
| `render_job_no_source` | A `Whole`, `Shots` or `Markers` range, or a marker cut, needs a file the job does not name. |
| `render_job_unknown_marker` | The sequence has no marker with that name. |
| `render_job_unknown_shot` | The shot list has no shot with that name. |
| `render_job_bad_path` | Unknown token, unmatched brace, empty part, `{shot}` without a shot list, or an invalid expanded name. |
| `render_job_path_clash` | Two takes expand to the same folder and stem. |
| `render_job_no_date` | A path uses `{date}` and no `date` was given. |
| `render_job_preroll_cap` | More frames would be rendered unwritten than `max_preroll_frames`. |
| `render_job_unbound` | A `bindings` entry names no entity: no entity has that `Name`, or none has that guid. |
| `render_job_bad_package` | A product job's package folder cannot be read, fails verification or does not open. |
| `render_job_build_refused` | A product job's build is not valid JSON, is another product's, or is no longer sold. |
| `render_job_pick_refused` | A product job's timed pick would be refused by the player on its frame. |
| `render_job_outside_view_range` | A frame's camera in a product job is outside the recipe's `view_range`. |

The render-out errors (`F0009` play.step running, `F0011` no renderer, `F0018` a job running,
`F0019` no ffmpeg) apply as for `render.out_start`. So do its AOV refusals, `unknown_aov` (an `aovs` entry that is not an AOV name)
and `aov_format_unsupported` (`png`/`png16` with an AOV that needs exact values); see
[Render-out](render-out.md).

## Not done yet

- No cheaper, inexact pre-roll: every earlier frame is rendered at full size.
- No per-take WAV files: one timeline WAV, with each take's sample range.
- `regia render --job` attaches to a running app; it does not launch one.
