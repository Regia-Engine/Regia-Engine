# Gizmos: debug overlays

Turn the engine's debug overlays on and off, and restyle any gizmo group, to see what the game
cannot show: collider shapes, the nav mesh, audio emitter ranges and AI senses. Pair it with
`seeing.screenshot` to check placement. Groups are listed by `debug.gizmos`; the overlays ship
disabled.

| Group | Draws |
|---|---|
| `physics` | Collider wireframes (green; sensors yellow). Convex hulls, height fields and very large trimeshes draw their bounds. |
| `nav_mesh` | Nav-mesh polygon outlines (cyan, lifted 5 cm) and off-mesh links (pink arrows). |
| `audio` | Emitter full-volume sphere (orange), silence sphere (brown), inner/outer cones and facing arrow. |
| `ai_perception` | Sight fan at eye height (red) and hearing ring (blue) for every `Perception`. |
| `smart_object` | Smart-object slots: available, claimed and disabled markers and facing rays ([Smart objects](smart-objects.md)). |
| `camera_rig` | Data-driven camera rigs: pose frustums tinted by weight, boom line, collision probe, aim point, layer markers. |
| `motion_matching` | Motion matching: the desired trajectory with facing ticks, the matched frame's own trajectory in gray, foot positions. |
| `post_volumes` | Post volumes: each volume's shape and blend band. |
| `default` | Whatever game code draws with `Gizmos`, and the `DebugDraw` queue: lines and world-space labels, each kept for its `duration` in frames ([Render lights](render-lights.md#debugdraw-durations-and-labels)). |
| `paths` | Current agent position through unvisited navigation waypoints; registered by `DebugGizmosPlugin`, off by default, with the project path color. |

## List groups

`debug.gizmos`:

```json
{}
→ { "groups": [
      { "name": "ai_perception", "enabled": false, "line_width": 2.0, "perspective": false,
        "style": "solid", "joints": "none", "depth_bias": 0.0, "on_top": false },
      { "name": "default", "enabled": true, ... },
      { "name": "nav_mesh", "enabled": false, "depth_bias": -0.01, ... },
      ... ] }
```

## Toggle and style a group

`debug.set_gizmos` needs `group` and at least one of `enabled`, `line_width` (0..=64 px),
`depth_bias` (-1..=1), `on_top`, `perspective`:

```json
{ "group": "physics", "enabled": true, "on_top": true }
→ { "before": { "name": "physics", "enabled": false, ... },
    "after":  { "name": "physics", "enabled": true, "on_top": true, ... } }
```

An unknown group is `NOT_FOUND` with the list of groups in the hint. The call is mutating and
needs the `edit_scene` permission (it is audited like other edits).

## Recipes

- **Is the trigger where the designer thinks?** Enable `physics` with `on_top: true`, take a
  screenshot from the player camera, look for the yellow sensor box.
- **Why does the bot walk around this ramp?** Enable `nav_mesh`; a missing outline over the ramp
  means it is not walkable at the current agent slope/climb.
- **Can the guard see the door?** Enable `ai_perception`; the red fan is exactly the cone
  `in_sight_cone` tests (horizontal, `sight_half_angle` either side of the facing).
- **Is the alarm audible from the spawn?** Enable `audio`; the brown sphere is `max_distance`.

The overlays draw after the scene and before the UI, so a screenshot includes them; turn them off
again before capturing reference images.

The `paths` group omits completed paths and non-finite edges and caps work per frame. It uses
`DebugColors.path` in a client build; the editor's **Show ▸ Paths** is a separate view flag
([Project debug colors and remaining paths](viewport-flags.md#project-debug-colors-and-remaining-paths-w12)).

## Sub-toggles: the locomotion group

The `locomotion` group (ADR-8729, off by default) draws walkers (`GaitRig`), foot planters
(`FootPlacement`) and their leg IK. It has three sub-toggles, all on by default: `gait`,
`foot_placement` and `ik`. `debug.gizmos` lists them as `toggles` on that group's entry, and
`debug.set_gizmos` takes `toggles` (an object of name to boolean; the names it leaves out keep their
value), alone or with the other fields:

```json
{ "group": "locomotion", "enabled": true, "toggles": { "ik": false } }
→ { "before": { "name": "locomotion", "enabled": false, ...,
                "toggles": { "gait": true, "foot_placement": true, "ik": true } },
    "after":  { "name": "locomotion", "enabled": true, ...,
                "toggles": { "gait": true, "foot_placement": true, "ik": false } } }
```

`toggles` that is not an object, an unknown sub-toggle, a non-boolean value, or `toggles` on any other
group is `invalid_params` and changes nothing.

A crowd cannot grow the overlay without bound: a frame stops starting new draws once it holds
`MAX_LOCOMOTION_SEGMENTS` (20 000, the paths overlay's budget) segments, counted conservatively, and
`LocomotionGizmoStats::skipped` counts the walker gaits, IK chains and foot planters it left out
(ADR-8800). A frame passes the budget by at most the one draw in progress. What each sub-toggle draws: [Procedural
gait](procedural-gait.md#see-it-the-locomotion-overlay) and [Foot
planting](foot-planting.md#see-it-the-locomotion-overlay).
