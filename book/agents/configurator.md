# Product configurator (agents)

The configurator runtime (ADR-2534, crate `regia_configurator`) holds one product and its current picks. It
reads the documents other tools publish (an option graph product record, a piece recipe, part kits, a
material library), refuses every combination the catalogue snapshot does not sell, tells a viewer what to
draw, and writes the `jewelry.build/1` JSON. It renders nothing and prices nothing. The file formats and the
contract checks are on [Product configurator contracts](configurator-contracts.md).

The capability marker `configurator_runtime_v1` in `agent.hello` says this build has the five methods below.

The core has no file system, thread or network: it reads through a `ContentSource` (a directory with the
crate's `fs` feature, bytes a web page fetched, an Android asset), so the same code runs in the browser
player and the Android viewer.

## Open a product

```json
{"method": "configurator.open_product",
 "params": {"product_key": "house-a/p0001", "root": "/var/lib/jewelry/out", "snapshot_id": "2026-11-04-000000000000",
            "library_version": "1", "kits": ["kits/shared/shared-basic/kit.json"],
            "tenant_id": "shop:example-shop", "tenant_houses": ["house-a"]}}
```

`root` is a content directory laid out like the plan's output folder:

```text
optiongraph/<snapshot_id>/houses/<house>/products/<product_slug>.json
recipes/<house>/<design_slug>.json          (optional: without it the product opens with no look)
kits/<house|shared>/<kit_id>/kit.json
materials/<library_version>/library.json
exchange/<name>/1/schema.json               (an owner's published schema; optional)
```

`option_graph`, `recipe` and `library` override a default path (a product package folder is
`option_graph: "options.json", recipe: "recipe.json", library: "materials.json"`); `documents: {option_graph, recipe?,
library?, kits?}` passes the files inline instead of `root`. With `validate` (the default) every file is
checked against its owner's published `schema.json` when the content directory has one, and the part kits
and library against Regia's own contract checks; a failure is `configurator_contract`.

A tenant sees only what it may: a `house:<slug>` tenant loads its own house and `shared` parts, a
`shop:<slug>` tenant the houses in `tenant_houses` (default tenant: `house:<the product's house>`).
`configurator_house_not_allowed` refuses anything else, including a kit or recipe of another house.

Opening refuses basic hardware (`configurator_not_modelled`: plain posts, pins and rings are never modelled
as products), a duplicate listing (`configurator_duplicate`, the message names the canonical one) and a
product with nothing for sale (`configurator_not_orderable`). Otherwise it starts from the **first
combination the snapshot sells**, so a viewer has something to show at once, and answers the state:

```json
{"product_key": "house-a/p0001", "listing": "complete", "flags": [],
 "picks": {"Gauge": "16g", "Gem": "Clear A", "Gem Size": "3x1.5mm", "Material": "Metal A", "Threading": "Threadless"},
 "resolution": {"orderable": true, "variant_key": "1001", "sku": "HA-0001-TL-A-16-3", "cart_mode": "shopify_lines",
                "cart_lines": [{"id": "1001", "quantity": 1}, {"id": "51", "quantity": 1}], "...": "..."},
 "geometry_key": "sha256:...", "camera": {"rig_preset": "front", "tilt_deg": 0, "...": "..."},
 "look": {"zones": {"body": {"kind": "metal", "material_id": "grey-metal-a", "finishes": []}, "...": "..."},
          "parts": [...], "gems": [...], "values": {"mq_l": 3.0, "mq_w": 1.5}, "unmapped": []},
 "read": ["optiongraph/...", "recipes/...", "kits/...", "materials/..."], "reopened": null,
 "view_range": {"tilt_max_deg": 30, "spin": "none", "...": "..."}}
```

**A truncated listing shows as incomplete.** When the record's status is `incomplete` or it carries the
`TRUNCATED_VARIATIONS` flag, every answer says `"listing": "incomplete"` and `flags` lists the codes. Its
listed variants still resolve (they are sold), but a viewer must never present the list as complete.

`build: {...}` reopens a saved `jewelry.build/1`: its picks, wear and camera are taken as they are (the camera
clamped and wear turns wrapped into the build bounds; a wear `preset_id` the build schema refuses is
`configurator_bad_document`) and re-resolved against the current snapshot. `reopened` says `{was_orderable, orderable_now, snapshot_changed,
recipe_changed, resolution}`; a build that no longer resolves is reported "no longer orderable", never
repaired silently. A build of another product is `configurator_wrong_product`.

## List the options

`configurator.options {}` lists every option group, axes first, then add-ons, in the record's order:

```json
{"group": "Gauge", "kind": "gauge", "role": "axis", "required": true, "multi": false, "applies": true,
 "look_only": false, "no_visual": true, "picked": "16g",
 "values": [{"label": "16g", "value_key": "Gauge=16g", "available": true, "selectable": true, "picked": true,
             "material_id": null},
            {"label": "18g", "value_key": "Gauge=18g", "available": true, "selectable": false, "picked": false,
             "material_id": null, "reason": "not_sold_with", "conflicts": ["Gem Size", "Material", "Threading"]}]}
```

A value is `selectable` only when some combination the snapshot sells has it together with every other
current pick: **the configurator never offers a pick that would not resolve**. An unselectable value says why
in `reason`: `unavailable` (the supplier no longer offers it), `not_sold_with` (a sold combination has it,
but not with the picks listed in `conflicts`, which an adjust-mode pick would change) or `not_orderable` (no
sold combination has it at all, for example a stone without a cart code). `applies` is false for an
add-on that does not apply to the current variant (a viewer hides it). `look_only` groups change only a
material (the record's `geometry_role` says so, else the group's kind and the recipe's drives); `no_visual` groups (the recipe's list) change nothing
visible. `material_id` is the material library entry a value maps to, when the recipe drives a material zone
with its group.

## Pick

```json
{"method": "configurator.set_pick", "params": {"group": "Gauge", "value": "18g", "adjust": true}}
```

`value` is a label, a list of labels for a multi group, or `null` to clear an optional add-on. In the default
strict mode a value no sold combination has with the current picks is refused with `configurator_not_sold`
and nothing changes. With `adjust: true` the value is kept and the configurator moves to the sold build
that changes the **fewest** other current picks (ties: the option graph's variant order); the changed picks
are listed in `dropped` and equal the value's `conflicts`. The search visits the explicit variant list once
and never enumerates combinations. A value no sold combination has at all is refused in both modes. Then
any open axis and any required add-on that now applies is filled from the first sold combination (`filled`).
The answer is the new state plus `dropped` and `filled`.

Refusals: `configurator_unknown_group`, `configurator_unknown_value`, `configurator_value_unavailable` (the
supplier no longer offers it), `configurator_pick_shape` (a list for a single-choice group, a string for a
multi group, a value twice), `configurator_not_sold`, `configurator_pick_required` (an axis or a required
add-on cannot be cleared, only changed) and `configurator_not_open`.

## What to draw

`look` in every state answer is render-independent: each recipe material zone's material (`grey-metal-a`; an
anodize pick drives the zone after the metal, so it wins; finish entries layer in `finishes`), each part
after any `part_swap` with its kit kind, file and node, each gem slot's cut type, size (with `$name`
values resolved), the kit `cut_id` of that type and size, and its stone's material, and the recipe's named
values after the picks. `unmapped` lists `group=label` picks the recipe drives but nothing maps, and parts or
cuts no loaded kit has (`part:<ref>`, `cut:<slot>:<cut>`); a viewer shows its fallback for those.

Each gem slot also carries `positions_norm` when the option graph record's slot of the same id has it (option
graph 1.2.0): one `[x, y]` per stone of the slot, on the supplier's blueprint drawing, x right and y down,
relative to the drawn stones' bounding box (center `[0.5, 0.5]`, longer side 1). It is `null` when the record
has none, as every 1.0 and 1.1 record. The loader checks it: two numbers per entry, each in `0..=1`, exactly
`count` entries, and slot ids unique; a record that breaks that fails to open with `configurator_bad_document`
naming the slot. It is a hint for photo overlays and capture tools; the 3D viewer places stones from the
package's gem slots, not from it.

```json
{"slot_id": "s1", "role": "position_1", "cut": "round_brilliant", "size_mm": [2.0], "count": 2,
 "cut_id": "rb-2.0", "material_id": "gem-black", "positions_norm": [[0.074, 0.627], [0.926, 0.627]]}
```

## Resolve

`configurator.resolve {picks?}` runs the shared `resolve()` on the open product: the given picks, or the
current ones. It changes nothing:

Group names and value labels match ignoring ASCII case; the answer carries the record's own spelling (a record
whose group names, or labels within one group, are equal ignoring case does not load:
`configurator_bad_document`, ADR-5587). Two
spellings of one group that pick the same values count as one pick; two that pick different values are refused
with `duplicate_value` (no spelling wins silently). A pick that chooses nothing, an empty label or an empty list for
a multi group, is no pick: an axis stays open, and a required add-on is still `addon_required` (an empty list never
sells a required multi group).

1. a product whose status is `not_orderable` or `duplicate` never resolves;
2. an unknown group, or an axis pick that is a list, unknown or no longer offered, refuses here;
3. exactly one live, priced variant must match every axis pick (a variant whose own axis value is no longer
   available is not offered). An axis left unpicked is fine when only one variant matches: the answer's `picks`
   fill it in;
4. the add-ons: one picked for a variant it does not apply to is refused. Of those that apply, a required group
   needs one value, an optional group none or one, a multi group any subset as a list; an unavailable value is
   never accepted;
5. pricing is not ported: Regia never prices, and the answer has no `cost` or `retail`;
6. a Shopify cart takes the variant and every chosen add-on value by its numeric code (a chosen value without
   one refuses the build); a manual supplier gets one made-to-order line (style SKU, variation id, metal code,
   each add-on's code in `attrs`, a list for a multi group);
7. the statement line: the title, then `<group>: <label>` for every axis of the variant, then every chosen
   add-on group, joined by ` | `; a multi group's labels are joined by `, `;
8. the geometry key, under which 3D assets are stored: `sha256:` of the compact JSON of the design key and the
   picks, by each group's `geometry_role` in the record (`geometry` when left out: its label; `gem`: the stone's
   cut, cabochon, size and count; `excluded`: nothing). So every metal, stone and finish swap of one shape
   shares a key once the record marks its groups. Option graph 1.2.0 adds two rules. A value with a
   `geometry_pick` object adds that object instead of its label (a style that differs from another only in its
   stones' material or color holds the same object, so the two share a key). A `gem` group with a `slot` enters
   as `slot:<slot>` instead of its group name, so the per-metal copies of one slot's stone list (`Gem 1` and
   `Gem 1 - Titanium`) key alike and a pure metal swap keeps the key. When two picked groups of one build claim
   the same slot, the first in record order takes `slot:<slot>` and the later one keeps its own name. Cut,
   cabochon and stone count stay geometry: a cabochon and a faceted stone on one setting are two models. A
   record without these fields keys as before. A refused build has no statement and no key:

```text
End piece | Threading: Threadless | Material: Metal A | Gem: Clear A
```

```json
{"method":"configurator.resolve","params":{"picks":{}}}
-> {"product_key": "house-a/p0001", "orderable": false, "variant_key": null, "sku": null, "picks": {},
 "cart_mode": "shopify_lines", "cart_lines": [], "made_to_order_line": null, "statement": null,
 "geometry_key": null, "open": ["Threading", "Gem Size"],
 "reasons": [{"code": "axis_open", "group": "Threading"}, {"code": "axis_open", "group": "Gem Size"}],
 "details": [{"code": "axis_open", "group": "Threading", "message": "3 variants match; pick `Threading`"},
             {"code": "axis_open", "group": "Gem Size", "message": "3 variants match; pick `Gem Size`"}],
 "listing": "complete"}
```

`reasons` is the shared format; `details` and `listing` are this method's additions. Resolution reason codes:
`product_not_orderable` (`value` holds the status), `unknown_group`, `unknown_value`, `value_unavailable`,
`wrong_pick_type` (a list for a single-choice group, a label for a multi group), `duplicate_value`, `not_offered`
(this combination is not offered), `axis_open` (pick the axes in `open`), `addon_required`,
`addon_not_applicable`, `no_cart_code`. `incomplete` is a build caveat, not a refusal: a truncated listing's
`configurator.build` answer lists it in `resolution.reasons` (with the resolution still orderable); the resolve
answer itself does not list it. The shared format also lists `no_price`; Regia never answers it, because
an unpriced variant is simply not offered.

The port is checked against a shared vector file (`exchange/option_graph/1/fixtures/resolve_vectors.jsonl`):
each line is `{product_key, picks, expect}` and every expected field must match exactly except the pricing
ones (`cost`, `retail`, `price`, `markup`). The crate's tests run a synthetic set whose expected answers come from
the owner's reference implementation; `REGIA_JW_EXCHANGE=<exchange dir>` runs the published file too, and
`tests/exchange_readers.rs` loads every published recipe, option graph and shop photo through Regia's own readers. Each published build must also come from the records' catalog snapshot and be exactly what `resolve()` answers for its picks: same resolution, `geometry_key`, `sku` and `house_variant_id`.

`tests/real_catalogue.rs` runs `resolve()` over a whole option graph folder at test time
(`REGIA_JW_GRAPH=<optiongraph/<snapshot_id> dir>`): every record must load; with `REGIA_JW_PILOT=<pilot.json>` every
pilot product's offered variants must resolve as the record says, a metal swap and a same-shape stone swap must keep
the key, and another shape must change it; `REGIA_JW_PILOT_VECTORS=<file.jsonl>` runs extra vectors against the
folder's records. Nothing of the catalogue is copied into the repo.

A recipe's `bake` block is not read: this viewer bakes nothing and draws the same with any flags, so a file may
set `transport_tables` and `probes` either way or leave the block out. It still counts toward the recipe's hash.

## Write the build

```json
{"method": "configurator.build", "params": {"wear_preset": "default", "output_gamut": "srgb"}}
```

`configurator.build` answers `{build}` (the camera has no depth of field unless given one; once the
compiled build schema declares the optional top-level `sku` and `house_variant_id`, they repeat the
resolution's): the `jewelry.build/1` of the current picks with the resolution, the
geometry key, the recipe's design, version and hash, the wear, and the camera clamped to the recipe's
`view_range` (tilt within `tilt_max_deg`; no spin unless the recipe allows it; pan within its bounds and within
±1000 mm; a focal length, sensor width, focus distance or f-stop that is not above zero resets to 100 mm, 36 mm,
60 mm or f/11 and a larger one clamps to 2000 mm, 200 mm or f/256; wear turns beyond ±360° wrap). There is
never a cost and never any client data. `build_id` defaults to an id derived from `build_hash`, so the same
build always gets the same id. `studio_id` picks the lighting set (default `studio-default`) and
`content_build_id` names the content build the viewer runs (default `unpackaged`). The build is checked against its contract before it is returned
(`configurator_build_invalid` would be an engine bug). A product without a recipe cannot build
(`configurator_no_recipe`); `configurator_unknown_wear` names a wear preset the recipe does not have.

## Errors

| Code | Meaning |
|---|---|
| `configurator_not_open` | No product is open. |
| `configurator_bad_document` | A document is not valid JSON, has the wrong schema id or misses a field the runtime needs. |
| `configurator_missing_file` | A document the request names is not in the content source (also when `root` does not exist, or a folder on the way is a file). |
| `configurator_bad_path` | A path is absolute, has `..`, `.`, an empty or dots-only segment, a backslash, `:`, a control character or a device name, or is over-long; or `root` is empty, has a control character, is over-long, or is a network or device form (`\\host\share`, `//host/share`, `\\?\`; stating one makes Windows connect to that host, ADR-5869). Nothing reaches the file system. |
| `configurator_io` | A document exists but could not be read, is not a regular file (a folder, a device, a FIFO), goes through a symbolic link below `root` (to a file or a folder; `root` itself may be a link), or changed between the check and the read. |
| `configurator_contract` | A document breaks its owner's published schema or Regia's contract checks. |
| `configurator_bad_options` | A build option or tenant field the caller passed (`studio_id`, `output_gamut`, `content_build_id`, `build_id`, `tenant_id`) breaks the `jewelry.build/1` contract; fix the caller's value. It is refused before the build is composed. |
| `configurator_house_not_allowed` | The tenant may not load that house. |
| `recipe_drive_unknown` | The piece recipe drives an option group the product's record does not have (the group and recipe are named); nothing is built from a recipe that aims at a missing group. |
| `configurator_not_modelled` | Basic hardware is never modelled as a product. |
| `configurator_duplicate` | The listing duplicates another. |
| `configurator_not_orderable` | The product has no combination the snapshot sells. |
| `configurator_unknown_group` | No option group of that name. |
| `configurator_unknown_value` | No value with that label. |
| `configurator_value_unavailable` | The supplier no longer offers the value. |
| `configurator_pick_shape` | A list for a single-choice group, a label for a multi group, or a value twice. |
| `configurator_not_sold` | No sold combination has that pick (with the current picks, in strict mode). |
| `configurator_pick_required` | An axis or a required add-on can be changed but not cleared. |
| `configurator_no_recipe` | No recipe, so no build. |
| `configurator_unknown_wear` | No wear preset with that id. |
| `configurator_wrong_product` | The saved build is for another product. |
| `configurator_build_invalid` | The build failed its own contract (an engine bug; report it). |
