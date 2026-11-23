# Product viewer contracts (`jewelry.*` JSON interfaces)

Decision records: ADR-2480, ADR-2535. Crate: `regia_configurator`. Agent guide:
[`book/agents/configurator-contracts.md`](../../book/agents/configurator-contracts.md).

A product viewer (a web player and an Android app: tap to switch gem and metal, drag to turn the view) is fed
by a kit builder written outside Regia and emits builds. At that boundary every file is UTF-8 JSON (sorted keys, LF) with a
`"schema": "jewelry.<name>/<major>"` field. The namespace `jewelry.` is the plan's; nothing in these formats
names a supplier. Regia may cook any of them into its own RON or binary forms inside.

The crate compiles copies of the schemas it owns from `crates/regia_configurator/assets/`, so a packaged crate needs
no `docs/` folder (ADR-1485). Edit both copies: a test requires them byte-identical.

## The interfaces Regia owns

| Interface | `schema` | Schema file | Page | Registry name | Version |
|---|---|---|---|---|---|
| Part kit (`kit.json`) | `jewelry.part_kit/1` | [`jewelry-part-kit.schema.json`](jewelry-part-kit.schema.json) | [part kit](jewelry-part-kit.md) | `jewelry-part-kit` | 1 |
| Material library (`library.json`) | `jewelry.material_library/1` | [`jewelry-material-library.schema.json`](jewelry-material-library.schema.json) | [material library](jewelry-material-library.md) | `jewelry-material-library` | 1 |
| Build | `jewelry.build/1` | [`jewelry-build.schema.json`](jewelry-build.schema.json) | [build](jewelry-build.md) | `jewelry-build` | 1 |
| Product package (`manifest.json`) | `jewelry.product_package/1` | [`jewelry-product-package.schema.json`](jewelry-product-package.schema.json) | [product package](jewelry-product-package.md) | `jewelry-product-package` | 1 |

Regia **reads** two interfaces it does not own: `jewelry.option_graph/1` and `jewelry.piece_recipe/1` (the kit
builder). Their schemas are the owner's published `schema.json`; Regia validates every such file against it
(§ Contract tests).

**Cancelled with the plan's v1 (ADR-2535):** the stream protocol, render requests and render-out manifests,
session records, gem transport tables and tenant content manifests. Regia does not ship, read or publish them;
`jewelry.stream/1` and the others answer `contract_unknown_interface`.

## Rules shared by every interface

- **Schemas** are JSON Schema 2020-12, self-contained (no remote `$ref`), with `additionalProperties: false`
  on every object: a field the schema does not name is refused, so `cost`, a customer's name, an e-mail or an
  IP address can never ride along. Free text appears only where a field's description says "Free text
  (marked)".
- **Versions.** The format version is the `<major>` in `schema` (`Interface::format_version`). Within a major
  a change may only add optional fields; anything else is a new major, and the old one stays readable for one
  release. Regia records each schema in `cargo xtask check-formats` (version = the major; fingerprint = the
  RFC 8785 text of the schema file), so an edit without a version bump fails the gate. `regia upgrade-project`
  lists every `*.json` file of these interfaces with its major and refuses a newer one. **Migration:** none
  yet, every format is at its first version; a new major ships its migrator in `regia upgrade-project`.
- **Units** are millimeters and degrees. The gem-local frame: origin at the girdle's center, `+z` toward the
  table, `+y` along the length (the point of a marquise or pear), `+x` across the width. Quaternions are
  `[x, y, z, w]`, unit length.
- **Keys** are natural keys (`house`, `product_key`, `design_key`, `variant_key`, `value_key`, `geometry_key`,
  `snapshot_id`), never database row ids. A tenant, where a file names one, is `house:<slug>` (one supplier) or
  `shop:<slug>` (a business), never a person.
- **Hashes.** `sha256:<64 hex>` for content hashes and `build_hash`; 64 hex digits for BLAKE3 (a package's files and
  `package_hash`). A contract hash is taken over the
  RFC 8785 (JCS) canonical form of the covered JSON: keys sorted by UTF-16 code units, no whitespace, numbers
  in the ECMAScript form of the IEEE double (`1`, `1.5`, `1e-7`; `1.0` and `1` are the same number). Any
  JCS library gives the same bytes.

## Refusals

A refused file answers with one or more errors `{ code, reason, path, message }`: `code` is the agent
protocol's F-code, `reason` a stable lowercase string, `path` a JSON pointer into the file. The schema
keywords map to the first group; the contract checks (each page's "Checks beyond the schema") add the rest.

| Reason | Code | Meaning |
|---|---|---|
| `contract_too_large` | F0002 | the file is over its byte limit (64 MiB; a kit 32,768 facet planes) |
| `contract_not_json` | F0002 | the bytes are not UTF-8 JSON (or nest deeper than the parser's 128 levels) |
| `contract_no_schema_field` | F0002 | no `schema` string, or not `jewelry.<name>/<major>` |
| `contract_unknown_interface` | F0002 | `schema` names an interface this engine does not know |
| `contract_wrong_interface` | F0002 | the file is another interface than the one asked for |
| `contract_unsupported_major` | F0002 | the major version is not one this engine reads |
| `contract_no_schema` | F0003 | an interface the kit builder owns, and no published schema was given |
| `contract_missing_field` | F0002 | a required field is absent (by the schema, or a conditional rule: a metal part's edge radius, a concave cut's mesh) |
| `contract_unknown_field` | F0002 | a field the schema does not define |
| `contract_wrong_type` | F0002 | a value of the wrong JSON type |
| `contract_bad_value` | F0002 | a value outside its enum or constant |
| `contract_out_of_range` | F0002 | a number outside its bounds |
| `contract_bad_string` | F0002 | a string that breaks its pattern or length (keys, hashes, relative paths, dates) |
| `contract_bad_count` | F0002 | an array or object with too few or too many entries, or a repeated entry |
| `contract_schema_mismatch` | F0002 | the value matches none (or several) of the allowed shapes |
| `contract_too_deep` | F0002 | the document nests deeper than the validator evaluates (64), or the schema needs more evaluations than its budget (1,000,000 plus 256 per value) |
| `contract_duplicate_id` | F0002 | an id, name, file or key that must be unique appears twice |
| `contract_duplicate_facet` | F0002 | two material entries claim the same option label |
| `contract_unknown_reference` | F0003 | a reference to an id the file does not define (a part's file, an anodize substrate) |
| `contract_wrong_kind` | F0002 | a reference to an entry of the wrong kind |
| `contract_bad_range` | F0002 | a min above its max, a default or step outside the range, an empty box |
| `contract_bad_transform` | F0002 | a quaternion or axis that is not unit length |
| `contract_bad_lods` | F0002 | LOD levels not increasing, or screen sizes not decreasing |
| `contract_bad_normal` | F0002 | a facet normal that is not unit length |
| `contract_cut_no_facets` | F0002 | a cut with neither facets nor a mesh |
| `contract_cut_unbounded` | F0002 | the facets of a convex cut do not close a solid |
| `contract_cut_empty` | F0002 | the facets of a convex cut leave no solid |
| `contract_cut_redundant_facet` | F0002 | a facet that touches the solid in no face |
| `contract_cut_size_mismatch` | F0002 | the solid's length or width differs from `size_mm` by more than 1 % (at least 0.01 mm) |
| `contract_cut_proportions_mismatch` | F0002 | the solid's crown height or pavilion depth differs from `proportions` by more than 1 point |
| `contract_bad_spectrum` | F0002 | spectral arrays of different lengths, or wavelengths not increasing |
| `contract_hash_mismatch` | F0002 | `build_hash` or `package_hash` is not the hash of the fields it covers |
| `contract_key_mismatch` | F0002 | a natural key that does not start with the file's house, or a recipe for another design |
| `contract_house_not_allowed` | F0002 | content of a house the tenant may not load |
| `contract_inconsistent_resolution` | F0002 | a resolution whose fields disagree, or a build's `sku` or `house_variant_id` that differs from it |
| `contract_package_incomplete` | F0002 | a package without exactly one recipe, options and materials file, or without a mesh |
| `contract_package_size_mismatch` | F0002 | `sizes.total` is not the sum of the listed files |
| `contract_package_file_missing` | F0003 | a file the manifest lists is absent |
| `contract_package_file_mismatch` | F0002 | a listed file's size or BLAKE3 hash differs from the manifest |
| `contract_package_has_cost` | F0002 | a JSON file of a package holds a cost, price or markup field |

## The validator

`regia_configurator::schema` (feature `validate`, on by default) is a JSON Schema 2020-12 validator written from
the public specification. A web player builds the crate without the feature, so `regex` stays out of the
player; `check` then runs the envelope and the contract checks but no schema, and the hashes and the `Interface` table
remain. The validator evaluates every applicator and validation keyword except `$dynamicRef`/`$dynamicAnchor`, `$anchor`,
`unevaluatedProperties`/`unevaluatedItems` and `contentSchema`; a schema that uses one of those, a remote
`$ref`, or a pattern the regex engine does not accept is **refused when it is compiled**, so a contract test
can never pass because a keyword was skipped. `format` is an annotation (the 2020-12 default). Patterns use
the Rust `regex` dialect (no look-around or back-references).

## Contract tests and the exchange

The owner of each interface publishes `exchange/<name>/<major>/{schema.json, CHANGELOG.md, fixtures/}` (plan
`interfaces.md` § 0.6). For Regia's interfaces those files are generated from this directory: `schema.json` is
byte-identical to `jewelry-<name>.schema.json`.

`crates/regia_configurator/tests/exchange.rs` validates every file Regia reads against the `schema.json` its
owner published, never a copy:

| Variable | Effect |
|---|---|
| `REGIA_JW_EXCHANGE=<dir>` (default `<out>/exchange`) | every Regia schema there equals this directory's; the kit builder's schemas compile; every fixture passes its schema; every package fixture's folder matches its manifest |
| `REGIA_JW_OUT=<dir>` (default `$JEWELRY_OUT`) | every `kits/**/kit.json`, `materials/*/library.json`, `recipes/**/*.json` and `optiongraph/*/houses/*/products/*.json` passes its published schema, and every package under `packs/` also matches its files |
| `REGIA_JW_PILOT=<pilot.json>` | the chosen product of every slot has an option graph record and a recipe naming it |
| `REGIA_JW_CONTRACT_REQUIRED=1` (or spec 42's `REGIA_JW_REQUIRE_PILOT=1`) | any missing input fails the test (ADR-278); without it the test reports what it did not find |

```sh
JEWELRY_OUT=/path/to/out REGIA_JW_PILOT=/path/to/pilot.json REGIA_JW_CONTRACT_REQUIRED=1 \
  cargo test -p regia_configurator --test exchange
```

## Differences from the plan's `interfaces.md`

Each is recorded in ADR-2535. `interfaces.md` still describes the cancelled v1 in places.

- **Part kit:** `max_bounces_hint` is optional and ignored (the viewer has no bounce loops).
- **Material library:** each kind's fields sit in a block named after the kind (`"conductor": {...}`; the finish
  block's kind is `finish_kind`). Only the fields the viewer reads are required; the physical fields of
  `interfaces.md` § 6 are accepted and ignored. Added: `roughness` on metals, `color_rgb_linear` on anodized
  entries (a plain color instead of a table), `tint_rgb_linear`, `sparkle` and `surface` on gems. A `fresnel_lut`
  may have 8 to 64 entries.
- **Build:** `tenant_id`, `studio_id`, `tier`, `output_gamut`, `front_still_ref` and `build_id` are optional, so a static
  page needs none of them; the camera's lens and `f_stop` and `dof` are optional too. `tilt_deg` is the angle from the front
  axis (0 to 90), with an optional `tilt_azimuth_deg` and `tilt_xy_deg`, because a tilt cone has two degrees of freedom.
  Added: optional top-level `sku` and `house_variant_id`, which must equal `resolution.sku` and `resolution.variant_key`.
- **Product package:** new, as `docs/specs/42-product-viewer.md` § 8.2 draws it; it replaces the tenant content manifest of
  § 12 for this track: one product per package, no entitlement list, no signature, BLAKE3 per file. `tenant_id` is optional.
