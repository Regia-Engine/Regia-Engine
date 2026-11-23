# Gizmos: debug lines and shapes

Gizmos are immediate-mode debug drawings: call a method from any system and the line or shape
is drawn for that frame. They render in their own pass after the scene (and the post stack and
translucent effects), before the UI, at every quality preset and on Android. The API is a port of
Bevy's `bevy_gizmos` (MIT OR Apache-2.0; see `docs/ported-sources.toml`).

```rust
use regia_core::math::{Color, Quat, UVec2, Vec2, Vec3};
use regia_render::gizmos::Gizmos;

fn debug_draw(mut gizmos: Gizmos) {
    gizmos.line(Vec3::ZERO, Vec3::Y * 2.0, Color::RED);
    gizmos.arrow(Vec3::ZERO, Vec3::X, Color::GREEN).with_tip_length(0.2);
    gizmos.sphere(Vec3::new(0.0, 1.0, -3.0), 0.5, Color::BLUE).resolution(48);
    gizmos.grid(
        Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
        UVec2::new(10, 10),
        Vec2::splat(1.0),
        Color::GRAY,
    );
    gizmos.text(Vec3::new(0.0, 2.5, 0.0), "spawn", 0.3, Vec2::ZERO, Color::WHITE);
}
```

Draw every frame to keep something visible. Draw from `Update` or `PostUpdate`: gizmos drawn in
`FixedUpdate` show only in frames where a fixed step ran.

## What you can draw

| Call | Shape |
|---|---|
| `line`, `line_gradient`, `ray`, `ray_gradient` | One segment |
| `linestrip`, `linestrip_gradient`, `lineloop` | Connected segments |
| `arrow` (`.with_double_end()`, `.with_tip_length()`), `axes`, `cross` | Pointers and markers |
| `circle`, `ellipse`, `sphere`, `arc_3d`, `short_arc_3d_between`, `long_arc_3d_between` | Round shapes (`.resolution(n)`) |
| `rect`, `rounded_rect`, `cube`, `cuboid`, `rounded_cuboid`, `aabb_3d` | Boxes |
| `capsule`, `cylinder`, `cone`, `conical_frustum`, `torus`, `tetrahedron`, `triangle`, `plane` | Primitives (axis along local +Y) |
| `grid`, `grid_3d` (`.outer_edges()`, `.skew()`) | Line grids |
| `frustum(view_projection)` | A camera's view volume |
| `curve_3d`, `curve_fn_3d`, `curve_gradient_3d` | Sampled curves |
| `text`, `text_sections` | ASCII stroke text (Simplex Hershey font) |

Placement arguments take anything that is an isometry: a `Vec3` (position), a `Quat`
(rotation), `(Vec3, Quat)`, an `Isometry`, a `Transform` or a `GlobalTransform`.

## Groups and styling

Every gizmo belongs to a config group. `Gizmos` (no type argument) is the `default` group;
`Gizmos<MyGroup>` draws into your own group. The `GizmoConfigStore` resource holds each group's
`GizmoConfig`:

- `enabled` — draw the group at all;
- `line.width` (pixels), `line.perspective` (width shrinks with distance), `line.style`
  (`Solid`, `Dotted`, `Dashed { gap_scale, line_scale }`), `line.joints` (`None`, `Miter`,
  `Bevel`, `Round(n)`) and `line.animation_offset` (march dashes);
- `depth_bias` in `-1..=1` — negative values pull lines towards the camera (use a small negative
  value for outlines lying on a surface);
- `on_top` — ignore scene depth so the lines show through walls.

```rust
use regia_app::App;
use regia_render::gizmos::{AppGizmoBuilder, GizmoConfigGroup, GizmoConfigStore};

#[derive(Default)]
struct PathGizmos;

impl GizmoConfigGroup for PathGizmos {
    const NAME: &'static str = "paths";
}

fn setup(app: &mut App) {
    app.init_gizmo_group::<PathGizmos>();
    let mut store = app.world_mut().resource_mut::<GizmoConfigStore>();
    if let Some((config, _)) = store.config_mut::<PathGizmos>() {
        config.line.width = 4.0;
        config.on_top = true;
    }
}
```

Guard expensive drawing systems with `.run_if(gizmo_group_enabled::<PathGizmos>)`.

## Retained gizmos

For geometry that does not change, build a `GizmoAsset` once and attach it with the `Gizmo`
component; it is drawn every frame at the entity's `GlobalTransform` with the component's own
line settings, depth bias and on-top flag.

```rust
use std::sync::Arc;

use regia_core::math::{Color, Vec3};
use regia_app::prelude::Commands;
use regia_render::gizmos::{Gizmo, GizmoAsset};
use regia_scene::Transform;

fn spawn_marker(mut commands: Commands) {
    let mut asset = GizmoAsset::new();
    asset.circle(Vec3::ZERO, 2.0, Color::YELLOW).resolution(64);
    commands.spawn((
        Gizmo::new(Arc::new(asset)),
        Transform::from_translation(Vec3::new(0.0, 0.1, 0.0)),
    ));
}
```

## Debug overlays

These groups ship disabled; enable them from code (`store.set_enabled("physics", true)`) or from
an agent (`debug.set_gizmos`, see the [agent page](../agents/gizmos.md)):

| Group | Draws | Drawn by |
|---|---|---|
| `physics` | Collider wireframes (sensors yellow) | `regia_framework::debug_gizmos::DebugGizmosPlugin` |
| `nav_mesh` | Nav-mesh polygon outlines, off-mesh links | `DebugGizmosPlugin` |
| `audio` | Emitter full-volume and silence spheres, cones | `DebugGizmosPlugin` |
| `motion_matching` | Desired and matched trajectories, foot positions | `DebugGizmosPlugin` |
| `ai_perception` | Sight fans and hearing rings | `regia_ai::AiPlugin` |
| `smart_object` | Slots by state, facing rays, entrances | `regia_ai::SmartObjectPlugin` (feature `debug_draw`) |
| `camera_rig` | Camera rig frustums, boom, collision probe, aim point | `regia_framework::camera::CameraPlugin` |
| `post_volumes` | Post volume shapes and blend bands | `regia_render::gizmos::GizmoPlugin` |

`DefaultPlugins` installs `DebugGizmosPlugin`; an overlay costs nothing while its group is off.

## Notes

- `DebugDraw` (the old single-method line queue) still works: insert it as a resource and push
  lines; they draw through the default group.
- Mirrors, UI preview cameras and reflection-probe captures never show gizmos.
- Colors are linear RGBA; lines are alpha-blended over the tonemapped image, so they are not
  affected by exposure, bloom or fog.
