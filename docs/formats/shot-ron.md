# .shot.ron: a list of look-review shots (v1)

Spec: `docs/specs/27-renderer-excellence.md` § 5 and `docs/specs/12-assets.md` § 4a (versioning).
Code: `regia_agent::shot_list` (`ShotList`, `Shot`). Example: [`shot.example.ron`](shot.example.ron),
parsed by a unit test. Decision: ADR-2463. Agent page: `book/agents/plates.md`.

A `.shot.ron` names the shots of a look review: for each one a camera, a time of day, a render
profile, a size, a warm-up and an AOV list. `seeing.capture_plate { shots_file, name }` renders one
shot of the list to a PNG or EXR plate plus a JSON sidecar. The list is data only: it holds no file
hashes and reads no clock.

Version **1**. A newer version is refused before the body is parsed. There is no older version, so
no migration step yet; when v2 arrives the loader runs the chain and `regia upgrade-project`
(format name `shot`) re-saves older files. The shape is hashed by `cargo xtask check-formats`.
Re-saving an unchanged file is byte-identical (`ShotList::to_ron`).

## Schema

```
ShotList {
    format_version: u32,              // 1
    name: String = "",
    shots: [Shot],                    // 1..=256, unique names
}
Shot {
    name: String,                     // 1..=64 of a-z 0-9 _ -, not leading '-'; the plate's file stem
    level: Option<String> = None,     // recorded in the sidecar; the host loads levels
    camera: Option<ShotCamera> = None,
    time_of_day: Option<ShotTime> = None,
    profile: Option<String> = None,   // a render.set_profile preset name
    width: Option<u32> = None,        // 1..=8192, default 1280
    height: Option<u32> = None,       // 1..=8192, default 720
    warmup_frames: Option<u32> = None,// 0..=600, default 8
    aovs: [String] = [],              // depth | id | motion, at most 3
    format: Option<String> = None,    // png (default) | exr
}
ShotCamera = Pose(eye: (f32, f32, f32), target: (f32, f32, f32), fov_y_deg: Option<f32>)
           | Named(String)            // the camera entity with this Name, left where it is
ShotTime { hour: f32 /* 0..24 */, day_of_year: Option<u16>, latitude_deg: Option<f32> }
```

A pose moves the scene's first camera (lowest `order`); the host's own camera systems must not
move it back during the warm-up. The time of day is written to the level's atmosphere clock
(`LevelSettings.atmosphere.time_of_day`).

## Migration note

v1 is the first version: nothing to migrate. Adding a field with a `#[serde(default)]` later is a
v2 with a no-op migration step (as `.render_job.ron` v1 → v2), recorded in `docs/decisions.md`.
