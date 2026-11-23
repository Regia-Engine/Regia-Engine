# Orbit, POV and Overhead spectator modes

The existing `spectator.get`, `spectator.set`, `spectator.step` and `spectator.bind`
methods now support five modes. They drive presentation only and keep working
while gameplay or replay is paused. Install `SpectatorPlugin` to write the pose
into a camera entity marked `SpectatorView`; pure camera logic needs no renderer.

| Mode | Target and controls |
|---|---|
| `free_fly` | Existing view-relative movement and look; world-up vertical movement. |
| `follow` | Existing smoothed anchor, orbit look and bounded distance. |
| `orbit` | Target stays centered immediately, including when it moves. Look orbits it; zoom changes the bounded distance. Without a target, the last anchor holds. |
| `pov` | Target position plus its rotated local eye offset. Full orientation, including roll, comes from the target. Movement/look/zoom do not change the view. Without a target, the last camera pose holds. |
| `overhead` | Looks straight down with screen up toward world −Z. Starts over the current target, or the camera position if no target is available. Movement pans in +X/−Z; vertical movement and look are ignored. Zoom changes height. The focus stays independent of subsequent target motion. |

`SpectatorMode::next()` cycles Follow → Orbit → Pov → FreeFly → Overhead → Follow.
Selecting the current mode again keeps its state; leaving and reentering Overhead
recenters on the current target. Following a game entity is optional: callers can
supply engine-coordinate poses directly without any game or simulation types.

## Drive and inspect

All new `spectator.set` parameters are optional:

| Parameter | Meaning |
|---|---|
| `mode` | One of the five strings above. |
| `target` | Finite world position `[x, y, z]` in meters. |
| `target_rotation` | Nonzero finite quaternion `[x, y, z, w]`, normalized by the camera. Uses the supplied/current target, or the origin if there is none. |
| `pov_offset` | Eye offset `[x, y, z]` in the target's local frame, in meters; defaults to zero. Entity scale does not scale this offset. |
| `overhead` | Full object `{height, min_height, max_height, pan_speed}`. Requires `0 < min_height <= height <= max_height`, `pan_speed >= 0`. Replaces tuning and resets current height. |
| `overhead_focus` | Explicit finite world focus `[x, y, z]`. |
| `clear_target` | Clears the target before any supplied target/rotation is applied. POV holds its last pose; Orbit/Follow hold their anchor. |

Defaults for Overhead: initial height 30 m, limits 1–1000 m, pan speed one current
height per second. These are engine defaults; hosts can choose their own values.
Diagonal pan is normalized. `spectator.step` uses `movement: [right, up, forward]`
and `zoom` in meters (positive outward), once on the first requested frame. The
existing frame clamp is 0.25 seconds; invalid/nonpositive frame time does not move
the camera. Pan is frame-rate independent for a fixed height and input. A POV eye
transform or Overhead step that overflows finite coordinates holds the last pose.

```json
{"method":"spectator.set","params":{"mode":"orbit","target":[3,2,-5]}}
{"method":"spectator.step","params":{"frames":30,"dt":0.016666667,"look":[0.02,0],"zoom":2}}
{"method":"spectator.set","params":{"mode":"pov","target":[3,2,-5],"target_rotation":[0,0,0,1],"pov_offset":[0,1.6,0]}}
{"method":"spectator.step","params":{}}
{"method":"spectator.set","params":{"mode":"overhead","overhead":{"height":24,"min_height":4,"max_height":96,"pan_speed":0.5}}}
{"method":"spectator.step","params":{"frames":60,"dt":0.016666667,"movement":[1,0,0],"zoom":4}}
{"method":"spectator.get","params":{}}
```

`get`, `set`, `step` and `bind` return the existing camera state plus
`target_rotation`, `pov_offset`, `overhead_focus`, `overhead_height` and `overhead`.
`overhead.height` is the configured reset height; `overhead_height` is the live
height after zoom. `rotation` is authoritative for POV roll and the vertical
Overhead pose. `yaw`/`pitch` are informational Euler angles and are not a full pose.

All parameters are validated before any state changes. Target updates precede
mode selection, so a target and Overhead mode in the same call center correctly.
An explicit `overhead_focus` applies last. `set` changes state; `step` or the next
plugin frame computes the resulting camera pose.

## Bind a scene camera and a subject

```json
{"method":"spectator.set","params":{"mode":"pov","pov_offset":[0,1.6,0]}}
{"method":"spectator.bind","params":{"camera":{"name":"Observer"},"follow":{"name":"Subject"}}}
```

Binding preserves Orbit, POV and Overhead; binding from FreeFly still selects
Follow. The plugin runs before transform propagation and composes the target's
current local transform chain, including a parent that moved this frame, so
Orbit centers that world position and POV uses that orientation in the same
frame. It writes the camera `Transform` in that system; propagation then
publishes the camera `GlobalTransform` before rendering. A chain that cannot
be composed (a parent without a `Transform`, a `ChildOf` cycle, or a chain
deeper than 64) falls back to the last propagated world pose. A parented
target at the world origin still uses its world pose. Bind and allow a plugin
frame before entering Overhead if its initial focus should come from that
entity. A disappearing entity retains its last sampled pose; a demo seek can
remap it and resume tracking. `clear_follow` stops refreshing the target;
use `clear_target` too to stop the remaining follow convergence.

Use `play.wait_until` / `play.expect` with the bound camera's `Transform`, or
`spectator.get` for pure headless logic. The dispatcher integration test
`spectator_modes_dispatch_bind_and_expect_the_render_camera_while_paused` exercises
these routes, verifies roll, pans while paused, and checks missing-target behavior.

## Permissions and errors

`get` requires `read`; `set`, `step` and `bind` require `play`. No new error codes.
Invalid mode, vector, quaternion or overhead settings return universal `F0002`
without partial mutation. An unresolved binding returns `spectator_bad_entity`.
See [demo recording](demo.md) for the existing binding and replay contract.
