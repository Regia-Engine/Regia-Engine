# Gameplay pause freeze

`Time::paused` is the one switch for a gameplay pause. While it is set, the game world freezes: not
only the systems that read `Time::delta()`, but every system that runs on a clock of its own
(ADR-10105). The pause key, the project setting and the default "Paused" screen come from the game
pause plugin; this page is about what the pause freezes and how to check it.

| Method | Permission | What it does |
|---|---|---|
| `play.pause` / `play.resume` | play | Set or clear `Time::paused`. |
| `play.freeze_state` | read | What the pause holds now: audio voices, the scene hold, a sim host, video clocks. |
| `render.set_scene_hold` | play | `follow_pause: true` makes the scene hold follow the pause. |

## What freezes

| Subsystem | While paused | On resume |
|---|---|---|
| Physics | The fixed loop takes no step. The interpolated `Transform` stays put: the blend is the banked overstep, which a paused frame does not change. Joints, character controllers, vehicles, cloth and ragdolls step only in `FixedUpdate`. | The run continues bit for bit where it stopped. |
| Audio | Voices on game buses pause in place (10 ms declick fade). A voice that starts on a game bus during the pause is held at once. Ducking, snapshots, voice chat and the `Ui` and `Music` buses keep running on real time. | Exactly the voices the pause held resume. A voice the game paused itself stays paused. |
| Video | Every clock mode stops, `Audio` and `Lockstep` included, and the player's audio feed holds. | The clock re-anchors, so the position does not jump by the paused time. |
| Threaded sim host | Held: it publishes a still pair and steps nothing. | Released, unless the game held it itself. |
| Coroutines | Frame waits (`wait_frames`, `next_frame`) do not count paused frames. Second waits already used game time. | The waits count on. |
| VFX | Particles, the VFX scene, decals, fire lights and debris follow `Time`. The entry streak holds its frame. | Continues from the held frame. |
| Animation, AI, navigation, rules, weather, water, materials | Driven by `Time`. The material clock gives `now == prev` while paused, so wind and scrolling stop and motion vectors are zero. | |
| Rendering | With the scene hold following the pause, the first paused frame renders the scene and keeps it. Every later frame draws only the UI over the kept image, so the world pixels stay identical and TAA/TSR history, jitter, noise and GI accumulation do not move. | The first frame is the one a never-paused run renders at the same renderer frame index. |

These keep running on real time, by design:
- UI (focus, gestures, scrolling, transitions, the soft cursor)
- the spectator camera
- `ShakeClock::Unscaled` shake
- the baked-GI stream fade
- network transport and voice chat
- loading, feedback and perf overlays
- the audio output gate and stall watchdog

Change which buses play through a pause with `Audio::set_pause_exempt_buses`. The default is `["Ui", "Music"]`, and each listed bus covers its child buses.

## Check a pause

```sh
regia agent call play.pause --project <project>
regia agent call render.set_scene_hold '{"follow_pause": true}' --project <project>
regia agent call play.freeze_state --project <project>
```

`play.freeze_state` answers:

<!-- not-a-request -->
```json
{
  "paused": true, "frame": 812,
  "audio": { "game_paused": true, "exempt_buses": ["Ui", "Music"], "held_voices": 6, "held_total": 6 },
  "scene_hold": { "on": true, "follow_pause": true },
  "sim_host": null,
  "video": { "players": 1, "held": 1 }
}
```

A subsystem the app does not have is `null`. `audio.game_paused` changes on the frame after
`play.pause`, because the audio system copies `Time::paused` in `PostUpdate`.

To prove the world is still, take a screenshot after the pause and another 300 frames (or 5 s)
later, then compare them outside the pause screen's region. With the scene hold following the
pause, the bytes are identical. Without it, the renderer keeps its temporal effects running, so TAA
jitter and noise still change pixels slightly under a still world. `render.scene_hold` reports
`follow_pause`, `paused` and each window's `held` frames.

## Errors

| Code | When |
|---|---|
| `F0003` | `play.freeze_state` in an app without a `Time` resource. |
| `F0018` | `play.pause` / `play.resume` during a render-out job. |
