# Camera shot sequences

A shot sequence is a camera move stored as data: a path through space, where the camera looks, its
field of view and roll, depth of field, and the cuts and fades between shots. It lives in a
`.shots.ron` file. The same file plays in a running game, renders frame-exact into a video
([Render-out](render-out.md)) and can be written by hand or by an agent
([agent page](../agents/camera-paths.md)).

The format reference is `docs/formats/shots-ron.md`; a complete example is
`docs/formats/shots.example.ron`.

## Why it is exact

`ShotSequence::sample(time)` is a pure function of the time. Seeking to 3.5 s gives the same pose on
every machine and every frame rate, and a fixed-step run reproduces bit for bit. Playback advances
the clock by the game's time step (`Time`), so pause, time scale and `play.step` all apply and no
wall clock is read.

## A first sequence

```ron
(
    version: 1,
    name: "intro",
    fps: 30.0,
    shots: [
        (
            name: "establishing",
            duration: 4.0,
            camera: (
                path: (
                    interp: CatmullRom,
                    keys: [
                        (time: 0.0, position: (0.0, 3.0, 20.0)),
                        (time: 2.0, position: (4.0, 3.0, 12.0)),
                        (time: 4.0, position: (6.0, 2.5, 6.0)),
                    ],
                    look_at: Point((0.0, 1.5, 0.0)),
                ),
                fov: [(time: 0.0, value: 35.0, ease: Smooth), (time: 4.0, value: 28.0)],
            ),
            fade_in: 1.0,
        ),
    ],
)
```

- **Shots** play back to back. Each has its own clock starting at zero. The boundary between two
  shots is a hard cut; a time exactly on a boundary belongs to the shot that starts there, and the
  end of the sequence holds the last frame.
- **Path interpolation** is `Linear`, `Step` or `CatmullRom` (the default; centripetal, so it cannot
  loop or cusp). Key times must strictly increase within `0..=duration`.
- **Look-at** is `Heading` (along the direction of travel), `Point((x, y, z))` or `Track([...])`
  (a moving target keyed like the path).
- **Tracks** (`fov` in degrees, 1 to 170, default 45; `roll` in degrees; and the depth-of-field
  `focus_distance` in metres and `aperture` in f-stops) hold the first value before the first key and
  the last after the last. `ease` on a key (`Linear`, `Step`, `Smooth`) shapes the segment that
  starts there.
- **Depth of field** is `dof: Some((focus_distance: [...], aperture: [...], focus_on_target: false,
  bokeh: false))`. `focus_on_target` uses the eye-to-target distance; `bokeh` selects hexagonal
  bokeh over a Gaussian blur. It needs a quality profile that allows depth of field.
- **Fades** (`fade_in`, `fade_out` seconds, `fade_color` linear RGB) are reported in every sample and
  drawn: the first time a fade is visible, `ShotPlugin` adds the compositor's fade layer to
  `WindowOverlay` (above the UI) and keeps it at the sampled opacity and color. Windows, screenshots with
  `window_layers` and render-out frames all show it. An app without a `WindowOverlay` gets the data only.
  `fade_in + fade_out` may not exceed the shot's duration.
- **Rotation keys** (`rotation: [(time: 0.0, rotation: (x, y, z, w), ease: Smooth), ...]`, format v2)
  key the camera's orientation as quaternions with a shortest-arc spherical interpolation. They replace
  the look-at rule for the camera's rotation; `roll` still applies and `look_at` still sets the sample's
  `target` for `focus_on_target`.

`ShotSequence::from_ron` validates the file and the message names the shot and the field. Saving
with `to_ron` is canonical, so an unchanged file re-saves byte for byte. The format is versioned
(v2; v1 files load and migrate); `cargo xtask check-formats` tracks its shape and `regia upgrade-project` migrates older files.

## Playing it in a game

```rust,wrap
use regia_framework::shots::{ShotPlayer, ShotPlugin, ShotSequence};
# use regia_app::prelude::App;
# let mut app = App::new();

app.add_plugins(ShotPlugin); // AgentPlugin installs the same player
let sequence = ShotSequence::from_ron(&std::fs::read_to_string("intro.shots.ron")?)?;
let mut player = app.world_mut().resource_mut::<ShotPlayer>();
player.load(sequence);
player.set_looping(false);
player.play();
```

`ShotPlugin` adds a system in `PostUpdate`, after the camera rigs and director and before transform
propagation. It writes the sampled pose, `fov_y` and depth of field to the camera:

- The camera is the one you pass to `set_camera(Some(entity))`, otherwise the `Camera` with the
  highest `order` (ties go to the lowest entity id).
- **Do not put a `CameraRig` on the camera a sequence drives.** The rig overwrites the pose.
- A shot change, a seek into another shot, a loop wrap and the first frame after a load are camera
  cuts. They are counted (`ShotPlayer::cuts`) and clear the renderer's temporal history
  ([camera cuts](../agents/camera-cuts.md)) so no frame blends the old shot into the new.
- With depth of field in the sample, the player sets the camera's `PhysicalCamera` focus distance,
  aperture and mode. If the camera has none, it adds one with motion blur off; an existing
  `PhysicalCamera` keeps its other settings.

To drive something other than the clock, sample by hand:
`sequence.sample(seconds)` or `sequence.sample_frame(frame)` (at the sequence's `fps`) return an
`Option<ShotSample>` with `eye`, `target`, `rotation`, `fov_y_degrees`, `dof`, `fade`, `fade_color`
and `finished`.

## Not included yet

Editing position paths and rotation keys in the editor (its Shots panel edits only the field of
view, roll, focus distance and aperture tracks; see [Shots panel](../agents/shot-panel.md),
ADR-1522), and cooking `.shots.ron` into paks. See the [agent page](../agents/camera-paths.md) for `render.sequence`, `camera.set_path` and
`camera.raycast`.
