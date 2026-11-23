# Part kit (`kit.json`, `jewelry.part_kit/1`)

Schema: [`jewelry-part-kit.schema.json`](jewelry-part-kit.schema.json). Shared rules, refusal reasons and the
differences from the plan's `interfaces.md`: [jewelry-contracts.md](jewelry-contracts.md). ADR-2480, ADR-2535.
Registry name: `jewelry-part-kit`. Format version 1 (the major in `schema`); no migrator yet.

The open-kit folder (`kit.json`, `.glb` files, `sockets.json`, `SHA256SUMS`, ADR-263) extended for a
product viewer. Path: `$JEWELRY_OUT/kits/<house|shared>/<kit_id>/`. Produced by the kit builder from
photos, published dimensions and parametric standard parts (there is no CAD); read and cooked by Regia.

## Fields

- **Top level:** `schema`, `kit_id` (names a folder: lowercase, digits, `_ . -`), `kit_version` (≥ 1),
  `regia_min_version` (`x.y.z`), `house` (a supplier slug, or `shared` for generic parts: posts, pins, balls,
  gem cuts, plain chains), `units` (`"mm"`), `parts[]`, `gem_cuts[]`, `files[]` (`{ path, sha256 }`, every
  file the kit uses).
- **`parts[]`:** `part_id`; `kind` (post, threadless_pin, end_base, setting, prong_head, bezel, chain, link,
  hoop, clicker, bead, clasp, jump_ring, bail, relief, tube, mesh_part); `params[]`
  (`{ name, unit: "mm", min, max, step, default, realized_by: variant_mesh | morph | bone_scale, variants?,
  target? }`); `glb` and `node`; `sockets[]` (`{ socket_id, kind: gem_seat | thread_male | thread_female |
  pin | jump_ring | chain_end | bail, transform { translation_mm, rotation_quat }, size_mm }`);
  `material_slots[]` (`{ slot_id, role: metal | gem | enamel | opal | other }`); `lods[]`
  (`{ level, screen_size, node, tri_count }`); `bbox_mm { min, max }`; `tri_count`;
  `edge_radius_min_mm` (required with a metal slot); `source` (parametric, photo, caliper, hand_sculpt);
  `accuracy` (front_exact, plausible); `front_axis`; `provenance { by, method?, numbers? }`.
- **`gem_cuts[]`:** `cut_id`; `cut_type` (round_brilliant, marquise, pear, princess, heart, oval, emerald,
  baguette, tapered_baguette, trillion, cushion, radiant, asscher, rose, cabochon, other); `size_mm`
  (`[length]` for round cuts, else `[length, width]`, an optional depth third); `proportions` (`table_pct`,
  `crown_angle_deg`, `pavilion_angle_deg`, `girdle_pct`, `length_to_width`, optional `crown_height_pct` and
  `pavilion_depth_pct`, both in % of the width); `facets[]` (`{ kind, normal, offset_mm }`, ordered, never
  decimated: the plane `normal · x = offset_mm`, the stone on the side `≤`); `convex`; `mesh_ref`
  (`{ glb, node }` or `null`; required when `convex` is false); `symmetry { n_fold, mirror }`;
  `girdle_finish` (polished, faceted, frosted); `max_bounces_hint` (optional, accepted and ignored: the viewer has
  no bounce loops).

Gem-local frame: origin at the girdle's center, `+z` toward the table, `+y` along the length, `+x` across
the width, millimeters.

The viewer draws a faceted stone from these planes: per-facet normals, no ray tracing. A cut with `cut_type:
cabochon` selects the cabochon shader and brings its shape as a mesh.

## Checks beyond the schema

- `part_id`, `cut_id`, file paths, and per part `socket_id`, `slot_id` and parameter names are unique
  (`contract_duplicate_id`); every part's `glb` and every cut's `mesh_ref.glb` is listed in `files`
  (`contract_unknown_reference`).
- A part with a metal slot has `edge_radius_min_mm` (`contract_missing_field`). A parameter has
  `min ≤ default ≤ max` and `step` inside the range; a `variant_mesh` parameter lists variants inside the
  range; a `morph` or `bone_scale` parameter names its `target` (`contract_bad_range`,
  `contract_missing_field`). `bbox_mm.min ≤ max` (`contract_bad_range`). Socket quaternions and `front_axis`
  are unit length (`contract_bad_transform`). LOD levels increase and screen sizes decrease
  (`contract_bad_lods`).
- **Gem cuts.** Facet normals are unit length (`contract_bad_normal`). A cut marked `convex` is clipped from a
  box by every facet plane in turn: the planes must close a solid (`contract_cut_unbounded`), leave one
  (`contract_cut_empty`), give every facet a face of positive area (`contract_cut_redundant_facet`), match
  `size_mm` within 1 % (at least 0.01 mm, `contract_cut_size_mismatch`) and, when given, the crown height
  above the girdle and the pavilion depth below it within 1 point (`contract_cut_proportions_mismatch`). A cut
  with no facets needs a mesh (`contract_cut_no_facets`). A kit holds at most 32,768 facet planes
  (`contract_too_large`), and one cut at most 512 (`contract_bad_count`; a finer shape is a mesh).

## Fixtures

`crates/regia_configurator/tests/fixtures/jewelry-part-kit/kit-shared-basic.json`: a `shared` kit with an end
base, a marquise bezel cup with a `variant_mesh` length, a four-prong head and a gem slot, plus a round
brilliant (2.0 mm, Tolkowsky: table 53 %, crown 34.5°, pavilion 40.75°, crown height 16.15 %, pavilion depth
43.08 %) and a marquise (4 × 2 mm) built as girdle, crown, table and pavilion planes through the girdle edge,
and an oval cabochon (convex, no facet planes, a mesh).
