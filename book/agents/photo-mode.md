# Leashed photo mode

`photo.enter`, `photo.set`, `photo.capture` and `photo.status` drive a presentation
camera. Install `PhotoModePlugin` to hide HUD layers and to write a camera marked
`PhotoView`. The camera never pauses a match, never writes sim input, and reads no
wall clock.

| Access | What opens |
|---|---|
| Offline, training, paused, dead or spectating | Free leash, 30 m by default |
| Live online match | Short leash, 12 m by default |
| Competitive | `photo_not_allowed` |

Both radii, the move speed, the fast multiplier, the look rate, the pitch limit,
the dolly step, the field of view, the roll, the exposure, the focus, the aperture
and the capture caps are settings. The defaults equal the source constants.

Actions are input-map names: `photo_toggle`, `photo_capture`, `photo_exit`,
`photo_move`, `photo_up`, `photo_look`, `photo_fast` and `photo_dolly`. This module
stores no key defaults.

## Calls

`photo.enter` takes optional `networked`, `competitive`, `paused`, `training`,
`dead`, `spectating` and `anchor`. `photo.set` clamps `fov_deg`, `roll_deg`,
`exposure_ev`, `focus_m`, `aperture_f`, `pitch`, `free_radius_m` and
`online_radius_m`, and stores `yaw`, `tick`, `game`, `map`, `hires`, `width`,
`height`, `anchor` and `grade`. A later `photo.enter` resets the eye.

`photo.capture` writes `<game>-<map>-t<tick>-<n>.png` into `directory` or the
platform pictures folder. A relative `directory` joins the project root; an absolute one
must lie under the project, the temp directory, an `output_roots` entry of
`.regia/agent.toml` or the session pictures folder (the output-root rule of
[Safety](safety.md), ADR-8795). The name carries the sim tick. Hi-res doubles the frame,
then fits the long side under 7680. An empty call while photo mode is closed
returns `photo_inactive` and writes nothing.

```json
{"method":"photo.enter","params":{"networked":true,"anchor":[0,2,0]}}
{"method":"photo.set","params":{"fov_deg":55,"tick":123,"game":"ridge","map":"north","grade":"NEUTRAL"}}
{"method":"photo.capture","params":{"directory":"shots","width":2,"height":2,"hires":false,"tick":123}}
{"method":"photo.status","params":{}}
```

`photo.status` is read-only. The other three need play permission.

## Errors

| Code | When |
|---|---|
| `photo_not_allowed` | A competitive match |
| `photo_inactive` | `photo.capture` while photo mode is closed |
| `photo_no_directory` | No pictures directory is set |
| `photo_capture_io` | The still could not be written |
| `path_outside_project` | `directory` leaves the allowed roots: `..`, a symlink leading out, or an absolute folder outside the project, the temp directory, an `output_roots` entry and the session pictures folder (ADR-8795; `F0002`, `data.path_reason` names the rule) |

`F0002` covers a bad parameter. Exposure on the render camera is the saved EV plus
the photo bias, snapshotted when photo mode opens and restored on exit.
