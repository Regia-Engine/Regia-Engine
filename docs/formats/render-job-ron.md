# .render_job.ron: a sequence-driven render-out (v4)

Spec: `docs/specs/12-assets.md` § 4a (versioning). Code: `regia_agent::render_job` (`RenderJob`). Example:
[`render-job.example.ron`](render-job.example.ron), parsed by a unit test;
the planner and runner are in `regia_agent::render_out` (`takes.rs`). Decision: ADR-1616. Agent
page: `book/agents/render-jobs.md`.

A `.render_job.ron` is a whole render-out request described by a sequence instead of frame
counts: the shot list it plays, the sequence whose markers it may name, the takes it writes
(each into its own folder and file stem), the cuts and every render-out setting. `render.out_job`
and `regia render --job` run it. The engine renders one timeline from sequence time 0 and writes
each frame into every take that holds it.

Version **4** (ADR-1880: the `bindings` table in v2; ADR-3369: `output.encode` in v3; ADR-2757: the
`product` block in v4). A newer version is refused before the body is parsed (`render_job_version`).
The loader runs the migration chain on older files: v1 → v2 only added the optional `bindings`,
v2 → v3 only the optional `output.encode` and v3 → v4 only the optional `product`, so an older job
loads unchanged and `regia upgrade-project` re-saves it at v4. (The `product` block was version 3 on
the jewelry line before it met ADR-3369's v3: both fields are optional, so a v3 file of either kind
loads and re-saves at v4.) The shape is hashed by
`cargo xtask check-formats`, and `regia upgrade-project` lists the format as `render-job`.
Re-saving an unchanged file is byte-identical (`RenderJob::to_ron`).

## Schema

```
RenderJob {
    version: u32,                         // 4
    name: String = "",                    // {job}, the manifest and WAV stem; "" means "job"
    shots: Option<String> = None,         // .shots.ron, project-relative
    sequence: Option<String> = None,      // .seq.ron, project-relative (markers; played with `bindings`)
    bindings: { String: JobBinding } = {},// v2: target name -> entity; non-empty plays `sequence`
    out: String,                          // the job's output directory
    path: String = "{take}/{take}",       // folders/stem template under `out`
    output: OutputSettings = (),          // the render-out settings
    takes: [Take],                        // 1..=256
    cuts: [CutAt] = [],                   // 0..=4096, added to the shot boundaries
    max_preroll_frames: Option<u64> = None,
    product: Option<ProductCapture> = None, // v4: draw this product, not the world
}
ProductCapture {
    package: String,                      // a `regia.viewer_package/1` folder, confined like `out`
    build: Option<String> = None,         // a `jewelry.build/1` file, confined like `out`: frame 0's picks and view
    options: [OptionChange] = [],         // 0..=4096, the option-change track
    view: [ProductViewKey] = [],          // 0..=4096, a camera path in `setView` terms (or give `shots`, in mm)
}
OptionChange {
    at: CutAt,                            // the pick applies before this frame draws
    group: String,
    value: PickValue,
    adjust: bool = false,                 // as `setPick`'s `adjust`
}
PickValue = One(String) | Many([String])
ProductViewKey {
    at: CutAt,                            // the frame the view reaches this key
    tilt: Option<(f32, f32)> = None,      // degrees; what a key leaves out stays at the key before it
    spin: Option<f32> = None,             // degrees
    pan: Option<(f32, f32)> = None,       // mm
    zoom: Option<f32> = None,             // 1 is the default framing
    ease: Option<String> = None,          // "smooth" (default), "linear" or "hold"
}
Take {
    name: String,                         // unique; 1..=64 of [A-Za-z0-9_.-], not leading '.'
    range: TakeRange,
    path: Option<String> = None,          // this take's template (default: the job's)
}
TakeRange =
      Whole                               // the shot list, else the sequence
    | Frames(start: u64, end: u64)        // timeline frames
    | Seconds(start: f64, end: f64)       // sequence seconds
    | Markers(from: String, to: String)   // .seq.ron markers
    | Shots(first: String, last: String)  // start of `first` .. end of `last`
CutAt = Frame(u64) | Seconds(f64) | Marker(String)
JobBinding = Name(String)                 // the lowest entity with this Name (by its guid if it has one)
           | Guid(String)                 // the entity with this scene guid
OutputSettings {                          // every field optional; None = the render-out default
    width, height, fps, fps_den, supersample, subframes, bit_depth, writers: Option<u32>,
    warmup_frames, settle_frames: Option<u64>,
    shutter_angle: Option<f64>,
    format, layout, calibration, png_compression, aov_format: Option<String>,
    hud: Option<bool>,
    aovs: [String] = [],                  // depth, id, motion, normal, normal_view, roughness, metalness, base_color, material_id (ADR-3256)
    audio: Option<AudioSettings> = None,
    time_map: [TimeMapKey] = [],
}
AudioSettings { sample_rate: Option<u32>, format: Option<String>, stems: [String] = [], timeline: Option<String> }
TimeMapKey { frame: u64, scale: f64, ease: Option<String> = None }
```

The string-valued settings take the `render.out_start` spellings (`format: "png"`,
`layout: "landscape"`, …). `render.out_start`'s own parser checks them, so a job accepts exactly
what the protocol accepts. `time_map` frames are timeline frames.

## Rules

- **Half-open ranges.** A range holds its start frame and not its end frame. A time maps to the
  first timeline frame at or after it. Frame `f` is at `f · fps_den / fps` seconds, so frame 30
  at 30 fps is exactly 1 s. An empty range is refused (`render_job_invalid`).
- **Shot boundaries** are the first frames that show the next shot (the shot player's `locate`
  decides), and they are cuts.
- **Timeline length** is the latest take end, at most 1 000 000 frames. Frames that no take holds
  are rendered unwritten (pre-roll); `max_preroll_frames` caps how many.
- **Path templates**: tokens `{job}`, `{take}`, `{shot}`, `{frame}` (6 digits), `{date}`
  (YYYY-MM-DD, from the caller). Each `/`-separated part must expand to a valid name; the last
  part is the stem. Two takes may not expand to the same folder and stem.
- **Paths** in `shots`, `sequence` and `out` are relative to the project root, and confined like
  `render.out_start` `out`.
- **Binding table (v2).** With a non-empty `bindings` the job plays `sequence` itself: it reads the
  sequence's sub-sequences from their project paths (the names its `Sub` sections use), resolves
  every binding against the world before anything starts (`render_job_unbound` when a name or guid
  matches no entity), creates an external-clock sequence player with those bindings for the run and
  removes it at the end. The plan's `sequence_players` counts it and its `bindings` lists
  `{ target, entity }`. `bindings` without `sequence` is refused (`render_job_invalid`), as are an
  empty target or name, a malformed guid and more than 1024 entries. Without `bindings` the job
  drives the external-clock players the host created, as in v1.

- **Product (v4, ADR-2757).** With `product` the frames come from the product viewer's raster renderer, through
  the web player's core: no world, no game renderer, no path tracing. `shots` then is a camera path whose numbers
  read as mm in the piece's frame; giving both it and `product.view` is refused. A product job refuses `bindings`,
  `subframes` above 1, `bit_depth` 16, `format: "exr"`, `aovs`, `audio`, `time_map`, `warmup_frames` and
  `settle_frames` (`render_job_invalid`). `Whole` with no `shots` and no `sequence` runs up to the last timed event.
  The whole timeline is walked before anything starts: a pick the player would refuse is
  `render_job_pick_refused`, a camera outside the recipe's `view_range` on any frame is
  `render_job_outside_view_range`, a build that is not this product's or is no longer sold is
  `render_job_build_refused`, and a package that does not read is `render_job_bad_package`. Agent page:
  `book/agents/capture-hooks.md`.

## Example

```ron,ignore
(
    version: 1,
    name: "trailer",
    shots: Some("cinematics/trailer.shots.ron"),
    out: "renders/trailer",
    path: "{date}/{take}/{job}_{take}",
    output: (width: Some(1920), height: Some(1080), fps: Some(24), supersample: Some(2)),
    takes: [
        (name: "opening", range: Shots(first: "wide", last: "close")),
        (name: "tail", range: Seconds(start: 12.0, end: 15.5)),
    ],
    cuts: [Seconds(6.0)],
    max_preroll_frames: Some(600),
)
```

## v3: `output.encode` (ADR-3369)

`output` takes an optional `encode` block, the encoder settings of `format: Some("webm")` (AV1 and Opus
in WebM, written in-process; `book/agents/video-encode.md`). Every key is optional:

```
EncodeSettings {
    quality: u8 = 80,            // 1..=255, the AV1 quantizer (lower is better); used when bitrate_kbps is 0
    bitrate_kbps: u32 = 0,       // 0 or 50..=200000
    speed: u8 = 6,               // 0..=10
    keyframe_interval: u32 = 240,// 1..=600 frames
    audio_kbps: u32 = 128,       // 16..=510 (Opus)
    threads: u32 = 0,            // 0..=64; 0 = half the cores, 1 = byte-identical output
}
```

The job passes it to `render.out_start` as `encode`, which checks it (the job file parser does not);
`encode` with another format is refused there.


A product job (v4):

```ron,ignore
(
    version: 4,
    name: "swap",
    out: "renders/swap",
    output: (width: Some(1080), height: Some(1920), fps: Some(30), supersample: Some(2)),
    takes: [(name: "all", range: Seconds(start: 0.0, end: 6.0))],
    product: Some((
        package: "products/piece-a",
        build: Some("capture/piece-a.build.json"),
        options: [
            (at: Seconds(2.0), group: "Material", value: One("14K Rose Gold")),
            (at: Seconds(4.0), group: "Gem", value: One("Blue Sapphire (synthetic)")),
        ],
        view: [
            (at: Seconds(1.0), tilt: Some((10.0, -6.0)), zoom: Some(1.2)),
            (at: Seconds(5.0), tilt: Some((-8.0, 4.0))),
        ],
    )),
)
```
