# Regia Engine v{VERSION}, Linux x86_64 release binaries

<!-- Template for the release-binaries lane's LAUNCH.md (ADR-1965). Replace {VERSION} and {COMMIT}; keep
     the Red-Rouge checklist wording, which states the repeat contract of book/agents/render-out.md. -->

Commit: {COMMIT} (tag v{VERSION}, origin/main; file COMMIT). Rust 1.99.0, plain
`[profile.release]`, `cargo build --release --locked -p showcase -p regia_cli -p regia_editor`.
Binaries: `showcase` (scene host), `regia` (CLI and agent client), `regia-editor`. Checksums: SHA256SUMS
(`sha256sum -c SHA256SUMS`). Not signed, not published.

## Versions
- `./regia --version` prints `regia {VERSION}`.
- `showcase` and `regia-editor` have no `--version` flag. The equivalent is `agent.hello`, whose `engine`
  must be `{VERSION}` (see below).

## Showcase host (headless works on lavapipe or a GPU)
    ./showcase --write-example /tmp/scene            # procedural example scene
    PROJ=/tmp/proj; mkdir -p $PROJ
    ./showcase /tmp/scene --project-dir $PROJ --size 1920x1080 &
    ./regia agent call --project $PROJ agent.hello '{}'
GPU-free run: `VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json`. Add `--window` for a window.
A replay store outside the project: `--replay-root DIR` (repeatable, read only), or `replay_roots` in
`$PROJ/.regia/agent.toml`. Without it `replay.load` refuses a path outside the project, temp dir, working dir
and `output_roots` (`F0002`, `path_outside_project`).
Before the first render-out of a headless host: `render.pipeline_warm '{"offscreen":{"width":W,"height":H}}'`
(the job's render size), then poll `render.pipeline_warm '{}'` until `offscreen_warm.state` is `done`; without it the
first frame compiles its pipelines on the app thread and the app answers nothing for up to 30 s. Set
`REGIA_PIPELINE_CACHE_DIR` for the host so later launches compile less.
Exit codes: 0 done, 2 bad arguments, 3 scene refused, 4 no GPU adapter.

## Editor
Needs a display. Headless: `xvfb-run -a ./regia-editor --project $PROJ` (with the lavapipe ICD above if no GPU).
Optional: `--exit-after-frames N`, `--game <crate>`.

## Agent socket and quit
The app writes `$PROJ/.regia/agent/endpoint.json` (mode 0600) and a token file next to it; `regia agent call
--project $PROJ <method> '<json>'` reads them. `quit.request '{}'` answers `{handled_by:"engine", will_exit:true}`,
the app runs its clean exit and the connection closes.
`agent.hello` must report `"engine": "{VERSION}"`.

## Audio stems enum is read live
`agent.schema` -> `render.out_start` -> `audio.stems.items.enum` lists the buses of the running app (the
showcase answers Master, Music, Sfx, World, Ui, Voice, Ambience, Reverb; a project's config/audio.ron may add
more). Do not hardcode it: read it from the host you call. An unknown bus is INVALID_PARAMS.

## Red-Rouge checklist (from reports/rr-smoke-v023-launch.md; run against these binaries)
Showcase (scene <build-dir>, a project dir of your own):
0. `agent.hello`: capabilities include shot_fade_layer, render_out_aov_id, render_out_aov_depth,
   render_out_aov_motion, shot_rotation_keys, render_out_audio, render_out_audio_stems; shots_ron_version 2.
1. Acceptance: 5 s at 120 fps, time_map scale 0.25, cuts [0,300]; manifest regia.render_out/2, frame_times.
2. Audio + stems: files.audio = mix then stems, samples = frames*rate/fps; same call twice gives the same hash.
3. AOVs/EXR: depth, id, motion; motion_reset_frames at cuts. `frame_hashes` and the AOV hashes repeat for the
   same request from the same world state: two fresh hosts, or one host whose world did not move (`time_map`
   scale 0 on every key, live effects included: such a job puts them back, ADR-2633; or a replay armed at the
   same tick). A second job in one showcase host otherwise continues the director's timeline and differs; a live
   effect (`vfx.crash`) moves on with it: the color frames differ, depth, id and motion repeat
   (book/agents/render-out.md, What repeats).
4. Fade + rotation keys: render.sequence export has version 2; manifest `fade` frames. Run
   `render.sequence {"action":"clear"}` before later jobs (a finished faded sequence renders black).
5. Replay scene (replay.load, replay.play start render_out): replay_from_tick in the manifest. The replay
   must lie under an allowed root (the project, or a `--replay-root` / `replay_roots` directory).
Editor (RR-6): put a manifest in $PROJ/.regia/tools/red-rouge.tool.toml; `agent.hello` has capability
editor_tools; then editor.tool_list, editor.tool_launch {"tool":"red-rouge"},
editor.tool_methods, editor.tool_call, editor.tool_stop (the token is revoked afterwards; `editor.tool_status`
then reads `exited` with `connected: false`).
