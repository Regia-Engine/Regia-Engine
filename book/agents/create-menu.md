# Create menu and viewport helpers

The editor's **Create** menu places one entity at the viewport's focus point (the orbit target),
selects it and pushes one undo step: **Shape ▸** a greybox primitive, or one of the placeable
actors **Player start**, **Trigger volume**, **Point light**, **Spot light**, **Directional light**,
**Camera**, **Reflection capture**, **Audio emitter**, **Rect light**, **Tube light**, **Decal**,
**Post-process volume**, **Spline**, **Nav link** and **Smart object**. An agent does the same with `editor.create_entity`; both
paths call `regia_editor::shapes::create_kind`, so the menu and the method make the same entity.
Spec 19 § 2 and § 3, ADR-7468, ADR-7592, ADR-7741.

## `editor.create_entity`

Params: `kind` (required), `translation` (`[x, y, z]`; default the focus point).

| `kind` | What it makes (named after the entry) |
|---|---|
| `player_start` | `regia_framework::player::PlayerStart`: the default spawn logic picks the first one. |
| `trigger_volume` | A sensor `regia_physics::Collider`: a 2 m box (`half_extents` 1) that raises collision events. |
| `point_light`, `spot_light`, `directional_light` | The default `regia_render` light; a spot light is turned to point at the floor. |
| `camera` | A default `regia_render::Camera`. |
| `reflection_capture` | A `regia_render::LocalCapture` with an id no other capture in the level uses (one past the largest, from 1) and a 4 m box (`half_extent` 2, the default 0.5 m blend margin); bake it with Tools ▸ Bake reflection captures ([Reflection captures](reflection-captures.md)). |
| `audio_emitter` | A default `regia_audio::AudioEmitter` (no sound yet: set `sound_name` or `cue_name` in the Inspector). |
| `rect_light`, `tube_light` | The default `regia_render::RectLight` / `TubeLight`; a rect light is turned to face the floor (it emits toward its local −Z), a tube light lies along its local X. |
| `decal` | A `regia_vfx::StaticDecal` naming the built-in `scorch` prefab (an empty name draws nothing); identity rotation projects it along world +Y onto the ground. |
| `post_process_volume` | A default `regia_render::PostVolume`: a 2 m box, priority 0, no overrides (add overrides in the Inspector). |
| `spline` | A `regia_scene::spline::Spline` of two points at local X −2 and +2 (an empty spline draws nothing and has no curve to edit); every other field is the default. |
| `nav_link` | A default `regia_nav::NavLinkAnchor`: lands 2 m ahead along local −Z, both ways, jump. Moving or turning the entity moves the link. |
| `smart_object` | A default `regia_ai::SmartObject` with no definition (name a `.smartobject.ron` in the Inspector; slots are child entities). |
| `cuboid`, `sphere`, `capsule`, `cylinder`, `cone`, `conical_frustum`, `plane`, `torus`, `tetrahedron`, `ramp`, `prism` | The default primitive shape (Create ▸ Shape). |

Each new entity is named after its entry (`Audio emitter`, `Post-process volume`, ...), and the menu and the method
save the same level bytes (the new-entry rows of `shapes::tests`). There is no entry for a water plane or a fog
volume yet: neither is a saved entity component (ADR-7741), so `water_plane` and `fog_volume` are unknown kinds.

Returns `{ entity, guid, kind, name, undo_depth }`. An unknown kind is refused with `data.reason`
`create_unknown_kind` (the message lists the kinds).

```json
{ "method": "editor.create_entity", "params": { "kind": "player_start", "translation": [0.0, 0.0, 4.0] } }
```

`editor.actions { panel: "shell.create" }` lists the menu's actor entries (ids `create.<kind>`) with
`editor.create_entity` as their agent path.

Known limit: **Create prefab from selection** (the Outliner's row menu, [Create prefab](prefab-create.md)) gives the
members instance-derived guids. Component fields that hold an entity follow the members into the instance (ADR-8593),
but a reference *by guid* held in another file (a sequence binding that names a selected door, say) still names the
old guid after the prefab is made. Re-point such references by hand.

## Helpers in the viewport

With the **Icons** show flag (`editor.set_viewport { icons }`, on by default):

- A **player start** draws a flag icon and, on the floor, a 0.4 m circle at its feet with an arrow
  along its facing (local −Z flattened onto the floor), in lime (`overlays::ICON_PLAYER_START`). It
  keeps its proxy cube, so it can be picked in the viewport.
- A **trigger volume** (every sensor collider) draws its outline dashed in pink
  (`overlays::TRIGGER_VOLUME`). The **Colliders** overlay (`editor.set_viewport { colliders }`) draws
  sensors solid in its own color instead.

With the **Bounds** show flag (`editor.set_viewport { bounds }`, on by default) a selected
**reflection capture** draws its influence box in amber and its blend margin dashed in orange
([Reflection captures](reflection-captures.md)).

Game view hides them all, like every helper. `editor.capture_viewport` shows them in captures.

## Show ▸ Navigation

`editor.set_viewport { navigation: true }` (the **Show ▸ Navigation** toggle; off by default) draws
the baked navigation mesh (the world's `regia_nav::NavMesh`): every polygon's outline 5 cm above its
surface in blue (`overlays::NAVIGATION`) and every off-mesh link as an arrow in pink
(`overlays::NAVIGATION_LINK`), at most 50,000 segments. `editor.set_viewport` answers the flag as
`navigation`. With no baked mesh nothing draws.

Tests: `cargo test -p regia_editor --lib shapes overlays` (`create_entity_matches_every_menu_entry`,
`new_create_entries_save_the_same_bytes_by_menu_and_agent_and_load`, `every_actor_entry_is_a_registered_create_action`,
`trigger_volume_helper_is_the_sensor_outline_dashed`, `navigation_flag_draws_the_baked_floor`),
`cargo test -p regia_editor --test create_menu_helpers` and, on a Vulkan adapter (lavapipe),
`cargo test -p regia_editor --test create_helpers_capture` (`player_start_helper_shows_in_the_capture_and_not_in_the_control`,
`navigation_flag_changes_pixels_over_a_baked_floor`).
