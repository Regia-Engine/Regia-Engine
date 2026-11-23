# Build (`jewelry.build/1`)

Schema: [`jewelry-build.schema.json`](jewelry-build.schema.json). Shared rules and refusal reasons:
[jewelry-contracts.md](jewelry-contracts.md). ADR-2480, ADR-2535. Registry name: `jewelry-build`. Format version 1
(the major in `schema`); no migrator yet.

One configured piece: the picks, the view state, and what they resolve to. Regia makes it; the host page, the order
platform and the integrator read it. It never holds cost, a price or client data: every object refuses fields the schema
does not name.

## Fields

**Required:** `build_hash`; `house`; `product_key`; `design_key`; `geometry_key`; `picks` (option group → value label; a
multi group takes a list); `resolution { orderable, variant_key, sku, cart_mode: shopify_lines | manual_line,
cart_lines[{ id, quantity }], made_to_order_line, statement, reasons[] }` (`made_to_order_line` is `null`, text or
`{ statement, style_sku?, variation_id?, metal_code?, attrs? }`; a reason is text or `{ code, group?, value? }`, the codes
in the kit builder's option graph schema); `snapshot_id`; `recipe { design_key, version,
hash }`; `wear { preset_id, rotation_deg, mirror }`; `camera { tilt_deg, spin_deg, pan_mm, zoom, ... }`;
`engine_version`.

**Optional:** `sku` and `house_variant_id` (the catalog's variant id), `null` when not orderable; `build_id` (a UUID);
`tenant_id` (`house:<slug>` or `shop:<slug>`, which content it may load, never a person); `studio_id`; `tier` (a viewer
writes `interactive`); `output_gamut` (srgb, p3); `front_still_ref` (`null` in a viewer); `content_build_id`.

**`geometry_key`** is the key `resolve()` gives (rule 8 of the kit builder's option graph 1.1.0): the same for every metal,
gem color and finish of one shape, so it names the 3D asset. A refused build's resolution has no key, so its top-level key
follows the same rule from the picks it asked for. `resolution.made_to_order_line` is the single line Regia writes; the
schema also accepts the kit builder's object form, but Regia never writes it.

**`camera`** holds the view state. Required: `tilt_deg`, `spin_deg`, `pan_mm`, `zoom`. Optional: `tilt_azimuth_deg`,
`tilt_xy_deg`, `rig_preset` (front, pan, tilt, spin), `focal_length_mm`, `sensor_mm`, `focus_distance_mm`, `f_stop`, `dof`
(`false` in a viewer, which has no depth of field).

`tilt_deg` is the angle between the view and the recipe's front axis; `tilt_azimuth_deg` its direction around that axis
(0 = up); `tilt_xy_deg` the same tilt as the viewer holds it. The recipe's `view_range` bounds them; Regia clamps to it.

`variant_key` is the plan's name for the catalog's `house_variant_id`. A viewer writes the top-level `sku` and
`house_variant_id` too, so a host page can read them without knowing the resolution's shape.

**A combination the snapshot does not sell** is never shown as orderable: `resolution.orderable` is false, its
`variant_key` and `sku` are `null`, and `reasons` says why. A truncated listing resolves the same way, with a reason
saying it is incomplete. Resolving is the configurator runtime's job; this format only requires the result to agree
with itself.

## `build_hash`

`sha256:` + the lowercase hex SHA-256 of the RFC 8785 canonical JSON of

```json
{"camera": <camera>, "picks": <picks>, "product_key": "...", "recipe_hash": <recipe.hash>,
 "snapshot_id": "...", "wear": <wear>}
```

The same build always gives the same hash; `1` and `1.0` are the same number. `regia_configurator::checks::build_hash`
computes it and needs no optional feature, so a web player can.

## Checks beyond the schema

`build_hash` matches (`contract_hash_mismatch`); `product_key` and `design_key` start with `house`, and the recipe is
for the build's design (`contract_key_mismatch`); a `house:<slug>` tenant only builds its own house
(`contract_house_not_allowed`); the resolution agrees with itself (orderable ⇒ variant, SKU and no
refusal reasons; not orderable ⇒ reasons; `shopify_lines` ⇒ cart lines; `manual_line` ⇒ a made-to-order line and no
cart lines) and `sku` and `house_variant_id`, when present, equal `resolution.sku` and `resolution.variant_key`
(`contract_inconsistent_resolution`).

## Fixtures

`crates/regia_configurator/tests/fixtures/jewelry-build/`: `build-house-a.json` (a generic piece, orderable) and
`build-not-sold.json` (the same piece, refused).
