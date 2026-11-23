# A tiny replay scene to test replay-driven render-outs

The repository carries a two-second replay in the engine's replay format and a sample project folder around it,
so a tool (Red-Rouge) can try `replay.play { start: "render_out" }` followed by `render.out_start` without any
game data. It lives at `crates/regia_agent/tests/fixtures/replay_scene/`:

```text
replay_scene/
  replays/tiny.do27replay    60 ticks per second, ticks 0..=120 (two seconds), no game state
```

The replay has applied-input records only: no fulls, no moments, so `replay.load` reports
`verify.ok: false` for lack of a full to check. That is expected; the file exists to give the replay clock
something to walk. `crates/regia_agent/tests/render_out_replay_scene.rs` checks the committed file against the
writer and runs the steps below against a headless app on lavapipe.

## The steps

1. Open the project folder in the game or editor (the project directory is the folder with `replays/`). Copy the
   fixture folder somewhere and point the app's project directory at it.
2. Load the replay. `replay.load` reads under the [allowed roots](safety.md#request-paths) only (ADR-1769,
   ADR-1965): a relative path joins the project root; an absolute path must lie under the project root, the
   system temp dir, an `output_roots` or `replay_roots` entry of `.regia/agent.toml`, or a root
   the host added (`regia_agent::ReplayRoots`, the showcase's `--replay-root DIR`). `..` and a symlink leading
   out are refused with `F0002` `path_outside_project` before anything is read. A replay store outside the
   project goes in `replay_roots`. Here the replay is in the project, so either form works:

   ```json
   { "method": "replay.load", "params": { "path": "/abs/path/to/replay_scene/replays/tiny.do27replay" } }
   ```

   The answer has `end_tick: 120` and `header.tick_rate: 60`. The player is paused at tick 0.
3. Arm it for the next render-out. The replay waits at `from_tick` and starts on the job's frame 0; the
   warm-up frames hold it at `from_tick`.

   ```json
   { "method": "replay.play", "params": { "from_tick": 30, "start": "render_out" } }
   ```

   The answer has `armed_for: "next_job"` and `position.playing: false`.
4. Start the render-out. A relative `out` is joined to the project directory (see [render-out](render-out.md)).

   ```json
   { "method": "render.out_start", "params": { "out": "renders/take1", "name": "take1", "frames": 10, "fps": 60 } }
   ```

   At 60 fps and speed 1 the replay moves one tick a frame: frame `i` is at tick `30 + i`. Poll
   `render.out_progress`; it reports `replay_from_tick: 30`, and so does the manifest
   (`take1_render_out.json`).
5. `replay.pause` when the job is done. The replay stays loaded at the last tick.

Keep `from_tick + frames` (at the frame rate and speed you chose) below `end_tick`; playback stops at the end
tick. The same steps work with any real replay: only the path changes.
