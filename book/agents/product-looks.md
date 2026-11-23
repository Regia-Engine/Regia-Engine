# Product looks (agents)

`regia_looks` holds the cheap looks of the light product viewer (spec 42 § 4, ADR-2538): shiny metals,
anodized colors, sparkling faceted stones, cabochons, opals and frosted stones, all on WebGL2 limits
(uniform buffers and textures only, at most 4 environment samples per gem fragment, no ray tracing,
no bounce loops). Every material value is data from a material library
(`jewelry.material_library/1`); the crate ships a library of generic stand-ins.

## What is in the crate

| Part | What it does |
|---|---|
| `cuts` | Stand-in gem cuts from published proportions, in the part kit's gem-local frame (length along +y, width along +x, table toward +z, origin at the girdle's center); `GemCut::to_kit_entry` / `from_kit_entry` read and write a kit's `gem_cuts[]` entry (facet planes `{ kind, normal, offset_mm }`, written as the shortest decimals that read back as the same 32-bit floats, so a written kit rebuilds the same planes and mesh; the cut's own `proportions`; a read cut's `depth_mm` is its solid's extent along z, table to culet, even when the culet sits off the axis), so a real kit's stones build the same way: `round_brilliant` (Tolkowsky), `marquise` (2:1), `pear` (1.5:1), `princess`, `heart`. A cut is a list of facet planes in millimetres; `GemCut::mesh()` is the flat-shaded convex solid (the heart is two convex halves). |
| `mesh` | The vertex format (`position`, `normal`, `facet`) and generic parts: ring band, ball, cabochon dome, bezel cup. |
| `material` | `Library` (parse, `by_facet("Material=Titanium")`, `look(id, finish)`) and `LookMaterial` with `LookKind` = `metal`, `gem`, `cabochon`, `opal`, `frosted`. A clear stone on a cabochon cut draws as `cabochon` (`for_cut(false)`); a gem entry with `surface_roughness >= 0.25` draws as `frosted`. |
| `anodize`, `bake` | The offline anodize baker: one oxide film on the substrate's n,k, 32 entries over cos theta. `bake_library` fills every `anodized.fresnel_lut`. Never run per frame. |
| `env` | The fixed studio: a cube map with roughness mips and three key lights. The piece turns; the studio does not. |
| `gpu` (feature `gpu`) | `LookRenderer`: one pipeline per kind plus the background, built once; a material swap changes uniform data only (`pipelines_created()` stays constant). `render_still(scene, w, h)` renders offscreen and reads back RGBA8. `headless_device()` opens a device with the WebGL2 downlevel limits. |
| `viewer` (feature `viewer`) | The looks on the product viewer's renderer: `hooks()` replaces every family's hook in `ViewerConfig::hooks`; `create_material(renderer, look, stone)` makes a material slot (uploading an anodize table first); `mesh_data` converts a mesh; `studio_env` turns the studio into the viewer's `BakedEnv`. |
| `board` | `look_board(tilt_deg)`: the look board scene as data. |
| `agent` | The `looks.*` JSON surface below. |

## Agent methods (`looks.*`)

The agent server answers four methods (registered in `regia_agent`, `crates/regia_agent/src/looks.rs`). The data
methods answer from the built-in stand-in library and touch no GPU; `looks.render_board` renders on its own
headless device, never the game's frame. The same JSON handlers (`regia_looks::agent::handle`) serve a viewer
host that has its own transport (the web player's JS bridge).

`looks.render_board` requires `play` and is mutating because optional `path` writes a PNG
(ADR-4760). A read-only token is refused even without `path`; the default profile permits it.
The three data queries remain `read`.

| Method | Params | Returns |
|---|---|---|
| `looks.list` | none | `materials[]` (`material_id`, `kind`, `look`, `display_name`, `catalogue_facets`, `status`), `cuts[]`, `kinds[]` |
| `looks.material` | `material_id`, optional `finish` (a finish entry id), optional `faceted` (default `true`) | `kind`, `fragment_entry`, `params` (the four uniform `vec4`s), `detail` (F0, roughness and `grain_mm`, or index, Abbe number and attenuation, or opal body) |
| `looks.cut` | `cut_type`, optional `width_mm` (0.5 to 40, default 3), optional `kit` (default `false`) | `size_mm`, `depth_mm`, `facet_family` (`brilliant` or `step`), `pieces`, `facets` (count per `table`, `crown`, `girdle`, `pavilion`), `triangles`, `bounds_mm`, and with `kit` the cut as a part kit `gem_cuts[]` entry (`kit_entry`, `null` for the concave heart) |
| `looks.render_board` | optional `width`, `height` (1 to 4096, default 960 x 600), `tilt_deg` (-60 to 60, default 22), `close_up` (`x_mm`, `y_mm`, `fov_deg` 0.5 to 90), `path` (a PNG, output-root rule of `render.out_start`) | `hash` (the same inputs on the same driver give the same hash), `items`, `adapter`, `sample_count`, `stats` (`pipelines_created`, `draw_calls`, `triangles_drawn`, `material_writes`), `path`, `bytes` |

Refusals are `F0002` with `data.reason`:

| Reason | When |
|---|---|
| `looks_missing_param` | `material_id` or `cut_type` is missing. |
| `looks_material_unknown` | No library entry has that id (also a finish id that does not exist). |
| `looks_material_not_drawable` | The entry is a `finish` (a modifier, not a material), or an anodize substrate is not a metal. |
| `looks_material_missing_field` | The entry lacks a field its look needs (for example `conductor.f0_rgb_linear`). |
| `looks_library_parse`, `looks_library_schema` | The built-in library does not parse (a build defect). |
| `looks_unknown_cut` | `cut_type` is not a stand-in cut. |
| `looks_bad_size` | `width_mm` is not a number or is outside 0.5 to 40, or a still side outside 1 to 4096. |
| `looks_bad_value` | `tilt_deg` or a `close_up` value is not a finite number in range, `close_up` is not an object, `finish` is not a string, or `faceted` or `kit` is not a boolean. |
| `looks_unknown_field` | `looks.render_board` got a field it does not know. |
| `looks_render_failed` | The viewer refused the board or the still failed. |
| `looks_write_failed` | The PNG could not be written. |
| `looks_no_adapter` | The machine has no GPU adapter (`F0003`); on a headless host install the lavapipe driver. |
| `path_outside_project` | `path` leaves the output root. |
| `editor_headless` (`data.code`) | The host opened no GPU on purpose (a headless editor started with `--no-gpu`, ADR-5180): `-32004`, nothing is rendered and no device is opened. |

Through `regia_looks::agent::handle` directly the codes are the same without the `looks_` prefix
(`unknown_method` for a method it does not answer). The standalone renderer's errors are `gpu_no_adapter`,
`gpu_no_device`, `gpu_poll`, `gpu_readback` and `too_many_draws` (`LookGpuError::code`).

## The gem look in one paragraph

Per fragment: one reflection of the studio with Schlick Fresnel from n_d; then for each RGB channel (three
indices spread by the Abbe number times a fire factor) the view ray refracts into the stone, reflects off the
pavilion facet it reaches (the pavilion is the 40.75 degree cone cut into 8 mains and 16 lower halves by
azimuth), once more if it still runs down, and leaves through the crown facet above (table, stars, bezels,
upper halves), or leaks dark out of the pavilion. Beer absorption over about twice the depth tints it; the three
key lights add glints on the facet and along the exit rays. No noise and no history: a still view renders the
same bytes every frame, and while dragging the pattern moves facet by facet.

A princess takes the **step** facet family (`looks.cut` reports it): a four-sided pyramid in chevron tiers
instead of the cone, up to three bounces, tinted per pane by the facets its ray met, a wider key-light lobe so a
whole facet flashes as it turns past a light, and a cap on the surface reflection so a softbox in the table
stays a veil over the pavilion. A kit `cut_type` this crate does not know draws with the brilliant family.

A low-index stone also gets a small constant lift in its dark zones, since it leaks studio light through its
pavilion. A pale quartz gets more of it, so a citrine center reads as yellow rather than olive; the extra step
follows the material's strongest absorption, so a deep amethyst keeps its dark zones. Both values are estimates
by eye.

## Opals and blasted metal

Opal play of color is a fixed 3x3 search for the small domain under a point. The domains sit under the
surface, so they slide as the stone turns; each is a grating whose hue follows the view (Bragg, with the
swing widened so it reads on a small stone) and whose brightness follows how closely it faces the viewer.
Neighbors share a color zone. Domains too small to resolve fade out instead of shimmering. The size is
`gem_special.patch_scale_mm`.

A finish with `finish_kind: sandblast` and a `feature_scale_mm` draws a grain of that size (`detail.grain_mm`):
smooth noise in the mesh's own space, so it turns with the piece, and it fades when it falls below a pixel.
Pits shade the studio (`1 - 1.2 x roughness`, an estimate by eye). Other finishes pay for one untaken branch.

## Checking a look

```text
cargo run -p regia_looks --features viewer --example look_board_viewer -- board.png 1920 1200
cargo run -p regia_looks --features gpu --example look_still -- board-standalone.png 1920 1200
cargo test -p regia_looks --all-features -- --nocapture
```

`look_board_viewer` draws through `regia_viewer` (what the web player and the Android viewer show);
`look_still` uses the crate's standalone renderer. Both read the same board and library.

The example renders the look board (every stand-in cut, clear and colored stones, frosted, cabochon, opals,
stones in bezel cups, ring bands in each metal and finish, anodized beads). Evidence images go to
`$JEWELRY_OUT/evidence/regia/`, never into the repository. `gpu_looks` and `viewer_looks` check that a still view is bit-stable on both renderers, that every looks hook compiles
for WebGL2 inside the viewer's prelude,
that swaps create no pipeline, that anodize colors read as named, `look_v2` measures the corrections of ADR-2587
on rendered pixels (flat panes under a princess table, opal fleck size and hue swing, gold saturation, blasted metal
level and stability), and that during a slow drag
(0.25 degrees a frame) at most 10 % of sparkle pixels are single-frame pops. With `REGIA_LOOKS_REQUIRE_GPU=1`
a missing adapter fails the tests instead of skipping them.

The library's stand-in values carry their `status`: measured F0 for fine gold, platinum, silver and titanium;
estimates fitted by eye for karat golds, rhodium, niobium, steel and every stone color. Regenerate the shipped
library after editing its source with `REGIA_LOOKS_BLESS=1 cargo test -p regia_looks --lib
bake::tests::builtin_library_is_the_bake_of_its_source`.

## In the players

The web player and the Android viewer draw these looks through `regia_web::scene::player_config` (ADR-2615). Read
`stats().looks` in a page, or `player.looks` in `viewer_app.stats`: `true` means every family runs its looks hook.

- **Studio:** the players use the viewer's `BakedEnv::studio_default()` (or a `studio` option) with this crate's three key
  lights, on the page's light background. The board stills use this crate's `Studio` on a dark background, so a board still
  and a player still of one stone differ. To judge what a customer sees, read a player still: `REGIA_WEB_EVIDENCE=<dir>
  cargo test -p regia_web --test scene evidence_stills_through_the_player`.
- **Stone size:** `StoneSize::from_bounds` reads the stone's mesh, whichever way its outline lies. `FacetFamily::of_cut`
  picks the pavilion from the cut's name, and a `cabochon` cut draws as a cabochon.
- **Stand-in cuts:** the stand-in kit takes its gem cuts from `GemCut::to_kit_entry`, the same full facet sets as the board.
  A cut the kits do not have still falls back to the cook's simplified set, which reads as large flat slices.
