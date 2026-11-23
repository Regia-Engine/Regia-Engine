# `<map>.weights.bin` (painted terrain layer weights, version 1)

Owner: `regia_terrain::layer_paint` (`LayerWeightMap`). The painted material-layer weights of a map, next to it and
named like its holes file (`maps/switchbacks.level.ron` → `maps/switchbacks.weights.bin`, `layer_weights_path`). One
8-bit weight per layer per heightfield sample, 0 = not painted; painted weight replaces procedural weight in proportion to
the painted coverage, so an unpainted map renders the procedural look. Written by `terrain.save_paint`, read by
`terrain.load_paint` (`book/agents/terrain-editing.md`).

| Field (postcard order) | Meaning |
|---|---|
| `version` | u32 varint, 1. Any other version is refused (`BadVersion`) |
| `width`, `height` | The heightfield's columns and rows |
| `layers` | Layer planes, 1 to the layer limit |
| `weights` | `layers` planes of `width × height` bytes, layer-major, rows top to bottom |

A blob whose weight count does not match `width × height × layers`, or larger than the sample limit, is refused.

**Versioning.** Registered in `cargo xtask check-formats` (`terrain-weights`) and `regia upgrade-project` (reads the
leading postcard version) since ADR-2316. A layout change bumps `version` and ships a migrator.
