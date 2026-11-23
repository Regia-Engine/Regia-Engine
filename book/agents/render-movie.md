# Render movie (editor)

**Sequence ▸ Render movie…** renders a saved `.seq.ron` of the project to frames or a movie file with the
editor's own renderer, as a cancellable editor job (ADR-7726). It runs the same job an agent runs with
[`render.out_job`](render-jobs.md): the dialog builds a `.render_job.ron` and starts it in process, so the
frames, their hashes, the manifest and the file names are that job's ([render-out](render-out.md)).

## The dialog

| Field | What it sets |
| --- | --- |
| Sequence | the project-relative `.seq.ron` (default: the file open in the [Sequence Curves panel](seq-curve-panel.md)); the **saved** file is rendered, so save edits first |
| Output folder | a project-relative folder inside the project (default `renders/movie`); anything else is refused with `path_outside_project` |
| Format | `png` (default), `exr`, `raw`, `webm`, `ffv1`, `prores` or `dnxhr` (the movie formats need the render-out's encoder or `ffmpeg`; the job refuses a format it cannot write) |
| Frames | from a frame to a frame (the end not included), or **to the end** of the sequence |
| Size | width and height in pixels (1 to 16384) and frames per second |

**Render** starts the job. The dialog then shows the frame count and a progress bar with **Cancel**; the editor
keeps drawing between frames (one output frame per editor frame; the files are encoded and written by the
render-out's writer threads). The run is also the background task `render_movie` in the status bar. Starting a run turns off the Sequence Curves panel's
**Preview in level** (its values go back first), so the edit-world preview never fights the job's player.

The job has one take named `movie`, the game UI off, and a binding table so it plays the sequence itself: the
Sequence Curves panel's bindings for that file (each by its entity's scene guid, else its name), plus every other
track target an entity is named after. Frames go to `<out>/movie/movie_<frame>.<ext>` (the job's default path)
with the manifest beside them.

**Cancel** ends the job at the next frame. What stays are the whole frames delivered so far, from the first one
up: a frame file is written as `<name>.part` and renamed, so no partial frame is left and no later frame is
written.

## Agent methods

- `editor.render_movie { sequence, out?, format?, start?, end?, width?, height?, fps?, dry_run? }` (needs
  `edit_scene`) starts the run the dialog's **Render** starts and returns `{ task, job, job_text, frames, out,
  state }`: `job` is the render-out job id (`render.out_progress { job }` reads it too), `job_text` the
  `.render_job.ron` it runs, `frames` the frames it renders. `start` is the first timeline frame (default 0) and
  `end` the frame after the last (default: the end of the sequence). With `dry_run: true` it checks the settings
  and returns `{ dry_run, job_text }` without starting anything.
- `editor.render_movie_status` (read-only) returns `{ task, job, state, frame, frames, first_hash, manifest, error,
  sequence, out, format }`; `state` is `idle`, `running`, `done`, `cancelled` or `failed`, and `first_hash` the
  first delivered frame's hash (the render-out's, hex).
- Progress: each frame the job delivers and its end add a `render_movie` record to `events.read`, whose `data` is
  the status above.
- Cancel: `editor.cancel_task { id: "render_movie" }` (the dialog's Cancel).

Sending `job_text` to `render.out_job` renders the same frames with the same hashes.

```json
{"method": "editor.render_movie", "params": {"sequence": "assets/cine/intro.seq.ron", "out": "renders/intro", "width": 1280, "height": 720, "fps": 24}}
{"method": "events.read", "params": {"since": 0}}
{"method": "editor.render_movie_status", "params": {}}
{"method": "editor.cancel_task", "params": {"id": "render_movie"}}
```

## Errors

Refusals answer `F0002` (`F0003` without a project) with `data.reason`:

| Reason | When |
| --- | --- |
| `render_movie_no_project` | no project is open |
| `render_movie_bad_sequence` | `sequence` does not end in `.seq.ron`, leads out of the project, cannot be read or does not parse |
| `path_outside_project` | `out` is empty, absolute, climbs out with `..` or leads out through a symlink |
| `render_movie_bad_format` | `format` is not one of the formats above |
| `render_movie_bad_range` | `end` is not after `start` |
| `render_movie_bad_size` | a size of 0 or over 16384, or a frame rate outside 1..=1000 |
| `render_movie_busy` | a render-out is already running |

The job's own refusals pass through with their [`render.out_job` codes](render-jobs.md) (`render_job_unbound`,
`render_job_no_source`, ...), and an editor started with `--no-gpu` answers `editor_headless`, as for
`render.out_job`. A run that fails ends `failed` with the render-out's error text in `error`.

## From Rust

`regia_editor::render_out_job::{job_for, start, tick, cancel, RenderMovieSettings, RenderMovieJob, MovieState}`;
`regia_agent::render_out_host::{out_job, progress, cancel, job_running}` call the render-out in process.

Not done: the dialog was driven by tests through its functions, not with egui input, and nobody has judged it on a
screen yet; it renders the saved file, not unsaved edits in the panel.
