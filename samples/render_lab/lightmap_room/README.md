# lightmap_room: a lightmapped level

A small project for the lightmap path (ADR-2635, `book/agents/lightmaps.md`): a room, a static
plinth inside it and a dynamic visitor. The level names its lightmap (`look.lightmap:
maps/room.flmap`); the file is not checked in (`.gitignore`). Bake it, then open the level:

```sh
cd samples/render_lab/lightmap_room
regia bake-lightmaps --scene assets/levels/room.level.ron          # writes assets/maps/room.flmap
regia bake-lightmaps --scene assets/levels/room.level.ron --gpu    # the same on the GPU baker
```

The static room and plinth get keyed entries (`entity_<guid>_<part>`); the dynamic visitor keeps
the probe light. Opening the level (`regia run --level`, `level.open`, the editor) installs the
lightmap and attaches every keyed part; `render.get_lightmaps` reports it under `level`.
`crates/regia_cli/tests/lightmap_sample.rs` bakes this project in a temporary copy and checks it.
