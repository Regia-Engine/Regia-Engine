# .camera.ron: data-driven gameplay camera (v2)

Spec: `docs/specs/35-camera-framework.md` §§ 3-8, ADR-1637. Code: `regia_framework::camera::asset` (`CameraAsset`).
Guide: `book/agents/camera-framework.md`. Registered as `camera` in `cargo xtask check-formats` and
`regia upgrade-project` (ADR-1654). Version 2 (ADR-3231) adds the `Rail` and `Crane` nodes; the v1 → v2 migrator only bumps the version
(`regia upgrade-project` rewrites the file with `format_version: 2`). A newer `format_version` is refused before the body is read; unknown fields and variants are load errors
with line and column. The shipped presets are `assets/camera/presets/*.camera.ron`.

`regia cook` validates the file (director hooks excepted: they are the game's code), fails on errors, and stores its canonical
RON text under the type path `regia_framework::camera::asset::CameraAsset`; there is no second binary form to version. A
package carries the file in the pak and as a raw file. Loading and hot reload: `regia_framework::camera::load`, ADR-2030.

## Schema

```
CameraAsset {
    format_version: u32,                       // 2 (1 is read and migrated)
    name: String,
    params: { String: ParamValue } = {},       // public parameters with defaults
    variables: [VarDef] = [],                  // runtime variables
    inputs: [InputSlotDef] = [],               // input slots (look axes, buttons)
    rigs: [RigDef],
    transitions: [TransitionDef] = [],         // shared transitions
    director: DirectorDef,
}
RigDef        { name: String, tags: [String] = [], root: [Node], transitions: [TransitionDef] = [], blend_out: Option<f32> = None }
TransitionDef { from: RigMatch, to: RigMatch, blend: Blend, start_facing: StartFacing = default, hold_outgoing: bool = false,
                share_state: bool = false, interruptible: bool = true, conditions: [Condition] = [] }
DirectorDef   = Fixed(String) | Rules(default: String = "", rules: [RuleDef] = []) | Ranked([RankedDef]) | Hook(String)
RuleDef       { when: Condition, rig: String }
RankedDef     { rank: i32, director: DirectorDef }
Value<T>      = Literal(T) | Param(String) | Var(String)
```

Nodes (v1 set): `Pivot`, `LookInput`, `Boom`, `Collision`, `Fov`, `Offset`, `SmoothPosition`, `SmoothRotation`,
`Lean`, `Frame`, `Shake`, `Group`, `When`, plus `Fly` (the free preset) and, from v2, `Rail` and `Crane`
(spec 35 § 17; v1 files migrate to v2 unchanged). Their parameters and behavior are in
spec 35 § 4.3; conditions (`Elapsed`, `VarBool`, `VarCompare`, `HasTag`, `RigIs`, `All`, `Any`, `Not`) in § 4.4;
transitions, blends and the director in § 6; layers in § 7.

## Example

```ron
(
    format_version: 1,
    name: "demo",
    rigs: [(name: "main", root: [Fov(degrees: Literal(70.0))])],
    director: Fixed("main"),
)
```

## Errors

Validation reports issues with a severity; an asset with an error does not load and the driver keeps the last good
version. Protocol codes (`CameraError::code`): `camera_bad_value`, `camera_asset_invalid`, `camera_unknown_rig`,
`camera_unknown_name`, `camera_unknown_layer_id`, `camera_order_taken` and the others listed in
`book/agents/camera-framework.md`.
