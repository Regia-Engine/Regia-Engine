# Framework: scripts, cameras, settings and the sim bridge

The gameplay-framework surface (`regia_framework`) is agent-readable through reflection: scripts
and their slots, camera rigs, live game settings, and the sim-bridge counters DO27's rule A1 checks.
Every call degrades gracefully — when the plugin a call reads is not installed, it answers `F0003`
with a hint naming the plugin, rather than pretending.

## Scripts

```json
scripts.list {}
→ { "scripts": [{ "type": "my_game::scripts::Door", "order": -7 }], "count": 1 }
```

`scripts.list` shows every registered script type and its execution order (from the
`ScriptRegistry`; needs `regia_framework::script::ScriptPlugin` and `register_script`).
`scripts.get` reports the script slots of one entity:

```json
{ "entity": 17 }
→ { "entity": 17, "slots": [{ "type": "my_game::scripts::Door",
     "order": -7, "started": true, "enabled": true, "running": true }] }
```

Slots attach through presentation scripts (the `KindRegistry`) or by spawning a `Scripts`
component. An entity without a `Scripts` component answers `F0003`.

## Cameras

`camera.get` reads the camera rig with the lowest entity id (or the one you name with `entity`) — its mode, yaw, pitch
and, for a chase rig, its target:

```json
camera.get {}
→ { "entity": 4294967296, "mode": "chase", "yaw": 0.0, "pitch": -0.2, "follow": "smoothed", "target": 4294967304 }
```

`camera.set_mode` switches a rig to a named mode and reports before/after:

```json
{ "mode": "overhead" }
→ { "entity": 4294967296, "before": "chase", "after": "overhead", "follow": "smoothed" }
```

`follow` (`smoothed` | `simulated`, optional on `camera.set_mode`, unchanged when omitted) picks which pose of a
predicted target the rig follows (spec 73 § 6.4, ADR-7213): the smoothed pose (the simulated pose plus the correction
offset, `NetVisualOffset`; the default) never pops on a correction; the simulated pose aims exactly at the predicted
state. A target without a correction offset has one pose. In code: `CameraRig::follow` (`CameraFollow`). An unknown
`follow` is `F0002`.

Modes: `chase`, `overhead`, `first_person`, `free` (aliases `third_person`, `top_down`,
`satellite`, `eye`, `spectator` accepted). Unknown modes are `F0002`. Needs
`regia_framework::camera::CameraPlugin` and an entity with a `CameraRig`.

## Settings

`settings.get` returns the whole `GameSettings` resource, or one reflected field via `path`:

```json
{ "path": "input.sensitivity" }
→ { "path": "input.sensitivity", "value": 1.0 }
```

`settings.set` writes one field, applies it live (frame cap → pacing, render scale and shadow quality → the render profile,
volumes → the buses, `display.resolution`, `display.fullscreen` and `display.vsync` → the primary window, see
[display settings](display-settings.md)) and persists it to disk. `settings.get` also answers `applied.display`, what
the window and its swapchain took.
`dry_run: true` deserializes the new value and returns `{ path, before, after }` without writing, applying or persisting it:

```json
{ "path": "audio.master", "value": 0.6 }
→ { "path": "audio.master", "before": 1.0, "after": 0.6 }
```

Engine-owned paths: `graphics.preset`, `graphics.shadow_quality`, `graphics.render_scale`,
`display.resolution`, `display.fullscreen`, `display.vsync`, `display.frame_cap`, `audio.master`, `audio.music`,
`audio.sfx`, `input.sensitivity`, `input.invert_y`, `language`,
`accessibility.subtitles`, `accessibility.text_scale`. Omit `path` to see them all; the change is
announced as a `SettingsChanged` message (readable with `events.read`).

An extension's settings resource (`declare_settings`, [extensions](plugins.md)) is reached by its extension id:
`{ "path": "dialogue.max_line_chars" }` reads one field and `{ "path": "dialogue" }` the whole value; answers add
`provider: "extension:dialogue"`. A `GameSettings` field of the same name wins. An extension settings write is live and is
saved to the user settings file under `extensions.<id>` (the RON text of the whole resource); at the next start the
saved value wins over `[extensions.settings.<id>]` of `Regia.toml` (ADR-7237). `settings.get { "path": "extensions" }`
shows what is saved.

Scene motion and the extra buses (ADR-3464) use the same methods. Further paths:
`accessibility.camera_shake_scale` (`0` to `1`), `accessibility.motion_blur` (a bool, or null to leave the
profile's bit), `accessibility.fov_override_deg` (degrees, or null for the camera's own value),
`accessibility.vibration_scale` (`0` to `1`), `accessibility.reduced_motion` (also zeros shake and head bob and
forces motion blur off), `audio.voice`, `audio.ambience`, `audio.mute_master`, `audio.mute_music`,
`audio.mute_sfx`, `audio.mute_voice`, `audio.mute_ambience`. `input.sensitivity` and `input.invert_y` apply only
when `config/input.ron` names the action in `look_actions`; otherwise the game reads them. The assist maps
(`accessibility.input_assists`, `input_assists_by_player`, `input_assists_by_gamepad`) are not reflected: set
them with `input.assists`.

## Sim bridge

`sim.counters` is the bridge instrumentation and the app clock — the raw material for DO27 rule A1
(`docs/specs/25` § 3), the frame-rate-independence check:

```json
sim.counters {}
→ { "frames": 240, "submit_input_calls": 240, "snapshot_reads": 240, "snapshot_ticks": 240,
    "events_drained": 3, "input_edges": 3, "elapsed_secs": 4.0,
    "reactions": { "vfx": 2, "vfx_unknown": 0, "decals": 2, "decals_unknown": 0, "sounds": 2,
                   "sounds_refused": 0, "announcer_lines": 1, "announcer_rejected": 0,
                   "unplaced": 0, "no_subsystem": 0 } }
```

`reactions` counts what the event router applied (`config/events.ron`): `Vfx` effects spawned and
`Decal` prefabs placed at the event's place, `SoundCue` voices started, `Announcer` lines started
or queued, and the misses: an unknown effect, prefab or cue (`*_unknown`, `sounds_refused`; a placed
cue past its range, 50 m unless the route gives `SoundCue` a `max_distance`, is refused too), a
rejected announcer line, a placed reaction of an event the sim gave no place (`unplaced`, see
`ExternalSim::event_place`) and a reaction whose subsystem is not in the app (`no_subsystem`).
Check a route: queue the event, `wait_until` the matching count rises.

`sim.check_framerate_independence` compares the sim-side tick rate of two measurement windows
taken at different frame rates and fails if they diverge beyond a tolerance (default 2 %):

```json
{ "a": { "ticks": 120, "wall_secs": 2.0 }, "b": { "ticks": 119, "wall_secs": 2.0 } }
→ { "a_rate": 60.0, "b_rate": 59.5, "ratio": 1.0084033613445378, "tolerance": 0.02, "pass": true }
```

`ratio` is `max(rate) / min(rate)`. `1.0` is a perfect match. `pass: false` means the simulation
advances faster or slower when the frame rate changes — a bug for a lockstep/rollback sim.
`sim.counters` needs `regia_framework::sim_bridge::SimBridgePlugin` (it reads `SimCounters`) and the
app clock. `sim.check_framerate_independence` does not read that plugin: it compares the two windows
in the call. With no `Time` resource it is `NOT_FOUND` (`no Time in this app`). The `sim_bridge` capability
in `agent.hello` appears when the plugin is installed.

## Threaded sim host

A game that runs its sim on `regia_framework::sim_host::ThreadedSimHost` and inserts the host's
`handle()` as a resource (`SimHostHandle`) gets three methods. Without the handle each answers
`F0003`.

`sim.host_status` reads the host's counters:

```json
sim.host_status {}
→ { "running": true, "panicked": false, "held": false, "manual": false, "tick_ns": 16666667,
    "ticks": 3600, "publishes": 3601, "curr_tick": 3600, "granted": 0, "debt_drops": 1,
    "ticks_dropped": 12, "client_waits": 0, "events_dropped": 0, "edges_dropped": 0 }
```

`ticks` counts steps since boot and `publishes` pair changes (a hold publishes one still pair);
`curr_tick` is the sim's tick in the newest frame. `debt_drops` counts times a realtime host fell
behind past its catch-up and restarted its clock, `ticks_dropped` the whole ticks skipped that way.
`client_waits` counts wakes that did not step because the presentation's frame stamp was stale.
`events_dropped` and `edges_dropped` count what the bounded event queue and input bus dropped (a
held host drops all input it is offered).

`sim.host_hold { held }` pauses (`true`) or releases (`false`) the host and answers
`{ held, was_held, status }`. A held host publishes a still pair (`prev` and `curr` the same
frame), so a screenshot of a held match is stable.

`sim.host_step { ticks? }` grants `ticks` (default 1, at most 1 000 000) more steps to a host
started with `HostPacing::Manual` and answers `{ granted, status }`; `granted` counts every grant
not stepped yet. The steps run on the host's thread: `wait_until` `curr_tick` reaches the target.
A held host keeps its grants until released.

| Code | When |
|---|---|
| `F0003` | No `SimHostHandle` resource in the app. |
| `sim_host_bad_ticks` | `sim.host_step`: `ticks` is 0 or above 1 000 000. |
| `sim_host_not_manual` | `sim.host_step`: the host steps on the wall clock (`HostPacing::Realtime`). Use `sim.host_hold` to pause it. |
| `sim_host_panicked` | `sim.host_hold`, `sim.host_step`: the sim panicked on the host thread and the host stopped (`logs.read` shows the panic). |
| `sim_host_stopped` | `sim.host_hold`, `sim.host_step`: the host thread has stopped; the game must start a new host. |

`sim.host_status` also reports the lockstep, snapshot and floating-point counters (ADR-8526):

```json
sim.host_status {}
→ { ..., "lockstep": true, "fp_env_supported": true, "fp_env_reset": false, "fp_checks": 3600,
    "fp_env_changed": 0, "frames_queued": 2, "frame_stalls": 1, "frames_refused": 0,
    "catch_up_budget": 0, "catch_up_ticks": 87, "snapshots": 1, "restores": 0,
    "last_step_ns": 412000, "max_step_ns": 1930000 }
```

### Lockstep pacing

A host started with `ThreadedSimHost::start_lockstep` (`lockstep: true`) steps exact per-tick
frames that a lockstep session pushes through its `LockstepFeed`: the frame for the next tick only,
each once and in order. `frames_queued` counts frames waiting; `frame_stalls` counts wakes that
found the next tick's frame missing and waited for it; `frames_refused` counts frames refused as a
duplicate, an old tick (`frame_out_of_order` in the Rust API) or past the queue cap.
`catch_up_budget` is the ticks a wake may step while the host catches up (0 when it is at the live
edge) and `catch_up_ticks` the extra ticks stepped that way. With `HostPacing::Lockstep { tick:
None }` the host is grant-paced (`manual: true`): `sim.host_step { ticks }` grants wakes, and a wake
steps one frame (or up to the catch-up budget). `last_step_ns` and `max_step_ns` time the sim's step
call alone; they are evidence, never a pass line.

A game whose sim implements `regia_detsim::replay::DetInputSim` wraps it in
`sim_host::DetInputHost<S, F, N>` to run it on this host (ADR-10062): the adapter owns the boundary
tick, steps through `replay::advance` (the step path a replay uses) and publishes frames of type `F`
(`DetInputFrame`, `()` for none). The frame queued for host tick `t` is the `TickInputs` of the sim's
tick `t - 1`. Its snapshot body is the boundary tick and then the sim's body, under the sim's tag with
the last byte's high bit flipped (`adapter_tag`), so `sim.host_snapshot` and `sim.host_restore`
work on it and a bare sim snapshot is refused with `bad_tag`.

### Snapshots and restore

`sim.host_snapshot { tick?, include_bytes? }` takes a framed snapshot (`regia_detsim::state`) on the host
thread, between two steps, and keeps it for `sim.host_restore`:

```json
sim.host_snapshot {}
→ { "tick": 5, "size": 56, "header_hash": "9c1f0e4b7d2a6c35", "tag": "counter",
    "state_version": 1, "kept": true }
```

`sim.host_snapshot { tick }` takes it at that tick boundary instead (ADR-10062): at once when the
sim is there, `sim_host_snapshot_tick_passed` when it is past it. For a tick still ahead the call
answers `{ "tick": 120, "pending": true, "kept": false }` and keeps the request; the host thread takes
the snapshot right after that tick's step, and the next `sim.host_snapshot { tick: 120 }` returns it
(still `pending` until then). A call with another tick replaces the request.

`header_hash` is the frame's body hash (hex): two snapshots of the same state have the same hash.
The bytes are never returned unless `include_bytes: true` in a debug build (`bytes_hex`); a release
build answers `host_snapshot_bytes_disabled`.

`sim.host_restore { bytes_hex? }` (debug builds only) loads a framed snapshot on the host thread,
replaces the sim, drops queued frames up to the restored tick and republishes a still pair there;
without `bytes_hex` it loads the snapshot the last `sim.host_snapshot` kept. It answers
`{ tick, status }`. A hold stays in place. Refused bytes leave the sim unchanged.

### FP environment

The host thread puts its floating-point environment back to the IEEE default before the first step
(`fp_env_reset: true` when it started under flush-to-zero, another rounding mode or traps, as a
thread spawned by an audio library's thread can) and checks it again every `fp_check_every` steps
(every step in debug builds, 600 in release builds): `fp_checks` counts the checks and
`fp_env_changed` the ones that found foreign code had changed it. `fp_env_supported` is `false` on a
target without a guard.

| Code | When |
|---|---|
| `sim_host_no_snapshots` | `sim.host_snapshot`, `sim.host_restore`: the host was started without snapshots (`ThreadedSimHost::start`, or a sim without `DetState`). |
| `sim_host_snapshot_failed` | `sim.host_snapshot`: the sim's `DetState::save` failed; the message names the det-state code. |
| `sim_host_snapshot_tick_passed` | `sim.host_snapshot { tick }`: the sim is already past that tick. |
| `sim_host_restore_failed` | `sim.host_restore`: the bytes were refused (bad frame, tag, version, hash or tick; the message names the det-state code, such as `bad_hash`). The sim is unchanged. |
| `sim_host_busy` | `sim.host_snapshot`, `sim.host_restore`: the host thread did not answer within 10 s (a long step). |
| `host_snapshot_bytes_disabled` | `sim.host_snapshot`: `include_bytes` in a release build. |
| `host_restore_disabled` | `sim.host_restore`: a release build never lets an agent replace the running sim. |
| `host_snapshot_none` | `sim.host_restore`: no `bytes_hex` and no kept snapshot. |
| `sim_host_panicked`, `sim_host_stopped` | As above. |
## Sim bridge presentation

The sim bridge (`SimBridgePlugin`) poses its entities by `SimBridgeSettings` and each kind's blend
policy (ADR-8456, [Handedness and blending](../systems/sim-bridge.md#handedness-and-blending)).
Without the bridge both methods answer `F0003`.

`sim.bridge_settings` reads them, with the blend counters since start:

```json
sim.bridge_settings {}
→ { "handedness": "left_handed_reflect_x", "teleport_distance": 10.0, "presentation_delay_ms": 16.666667,
    "default_blend": "lerp", "kinds": { "door": "hold", "hull": "lerp", "marker": "snap" },
    "stats": { "pair_reads": 600, "single_reads": 0, "lerped": 5400, "snapped": 600, "held": 590,
               "at_curr": 12, "teleports": 2 } }
```

- `handedness`: `right_handed` (nothing converted) or `left_handed_reflect_x` (the bridge reflects
  every snapshot pose and placed event across X once: positions `x → −x`, rotations
  `(w, x, y, z) → (w, x, −y, −z)`, yaw `θ → −θ`).
- `teleport_distance`: metres a `lerp` entity may move in one tick before it snaps to the newest
  pose; `null` never snaps.
- `presentation_delay_ms`: how far behind the newest publish the presentation draws (one 60 Hz tick
  by default).
- `kinds`: every kind in the bridge's `KindRegistry` and its policy; a kind not listed uses
  `default_blend`.
- `stats`: `pair_reads` and `single_reads` count frames the sim answered with a published pair or a
  single snapshot (a single snapshot is drawn as read: nothing is blended or counted below).
  `lerped`, `snapped`, `held` (drawn at the older frame) and `at_curr` (no older pose, or the weight
  reached the newest frame) count entity poses; `teleports` counts lerped entities snapped past the
  teleport distance. A manually paced `ThreadedSimHost` always draws its newest frame (`at_curr`).

`sim.set_bridge_settings { handedness?, teleport_distance?, presentation_delay_ms?, kind?, blend?,
dry_run? }` changes them from the next frame and answers `{ before, after, applied }` in the
`sim.bridge_settings` shape. `kind` and `blend` go together and set one registered kind's policy
(`lerp`, `snap` or `hold`). With `dry_run: true` nothing changes and `after` shows the result.

```json
sim.set_bridge_settings { "kind": "door", "blend": "hold", "dry_run": true }
→ { "before": { …, "kinds": { "door": "lerp" } }, "after": { …, "kinds": { "door": "hold" } }, "applied": false }
```

| Code | When |
|---|---|
| `F0003` | No sim bridge in the app. |
| `F0002` | `kind` without `blend`, or `blend` without `kind`. |
| `sim_bad_handedness` | `handedness` is not `right_handed` or `left_handed_reflect_x`. |
| `sim_bad_teleport_distance` | `teleport_distance` is negative, not finite, or not a number or `null`. |
| `sim_bad_delay` | `presentation_delay_ms` is not a number from 0 to 10 000. |
| `sim_bad_blend` | `blend` is not `lerp`, `snap` or `hold`. |
| `sim_kind_unknown` | `kind` is not in the bridge's `KindRegistry` (`sim.bridge_settings` lists them). |
