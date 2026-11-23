# Product viewer contracts (agents)

A product viewer (a web player and an Android app: tap to switch gem and metal, drag to turn the view) is fed by a
kit builder written outside Regia, which publishes part kits, piece recipes, option graphs and the material library.
Regia answers with builds and reads product packages. Every one of those files is JSON with a `schema` field of the
form `jewelry.<name>/<major>` (ADR-2480, ADR-2535). The formats, their rules and every refusal reason are in
[`docs/formats/jewelry-contracts.md`](../../docs/formats/jewelry-contracts.md); one page per format sits beside it.

The capability marker `configurator_contracts_v1` in `agent.hello` says this build has both methods below.

## Check a document

```json
{"method": "configurator.contract_check",
 "params": {"document": {"schema": "jewelry.build/1", "build_hash": "...", "...": "..."}, "interface": "build"}}
```

`configurator.contract_check` validates one document against its JSON Schema 2020-12 and then runs the
contract checks no schema can express:

- a convex gem cut's facet planes close a solid, every facet keeps a face, and the solid matches `size_mm`
  and the crown height and pavilion depth in `proportions`;
- `build_hash` and `package_hash` are SHA-256 over the RFC 8785 canonical JSON of the fields they cover;
- a build's `sku` and `house_variant_id` agree with its resolution, and a resolution agrees with itself;
- a `house:<slug>` tenant only holds its own house;
- a package holds one recipe, options and materials file and at least one mesh, and `sizes.total` adds up;
- references (a part's file, an anodize substrate) exist.

The answer is `{ ok, interface, schema_id, errors }`. An invalid document is **not** a call error: `ok` is
false and each entry of `errors` is `{ code, reason, path, message }`, where `code` is `F0002` (the file is
wrong) or `F0003` (it names something absent), `reason` is a stable `contract_*` string and `path` is a JSON
pointer into the document:

```json
{"method":"configurator.contract_check","params":{"document":{"schema":"jewelry.build/1", "...":"..."},"interface":"build"}}
-> {"ok": false, "interface": "build", "schema_id": "jewelry.build/1",
 "errors": [{"code": "F0002", "reason": "contract_unknown_field", "path": "/cost",
             "message": "`cost` is not a known field"}]}
```

`interface` (optional) refuses any other interface: give a wire name (`build`), a schema id
(`jewelry.build/1`) or a registry name (`jewelry-build`). The two interfaces the kit builder owns (`option_graph`,
`piece_recipe`) have no shipped schema: pass the owner's published `schema.json` content as `schema`, or the answer is `contract_no_schema`. `schema` also overrides
the shipped schema of a Regia interface (to test a draft).

`package_dir` (optional, with a product package manifest) is a folder relative to the project root under the
project-relative rule of [request paths](safety.md#request-paths): no `..`, drive, `:`, NUL or device name, and no
symlink leading out. After the manifest passes, each file it lists must exist with its size and BLAKE3 hash, and no
`.json` file may hold a cost, price or markup field. A listed file that leaves the folder or is not a regular file
counts as absent. Those refusals come back in `errors` like the others.

## Read the schemas

`configurator.contract_schema` without parameters lists every interface (`interface`, `schema_id`, `owned`,
`format`, `page`) and every refusal reason with its code and meaning. With `interface` it returns that
interface's shipped schema as JSON (`null` for the interfaces the kit builder owns).

```json
{"method": "configurator.contract_schema", "params": {"interface": "product_package"}}
```

## Errors

Both methods write nothing and read no world state (only `package_dir` reads files). Besides the universal codes,
`configurator.contract_check` answers `F0003`. A folder that is absent or cannot be read is answered by the kind of
failure, not in the operating system's words.

Validation against a `schema` has a work budget per call of 1 000 000 schema evaluations plus 256 per value of the
document (ADR-5597, one budget since ADR-5922): a schema whose `anyOf`/`oneOf`/`allOf`/`not`/`if` branches `$ref`
back up the tree would otherwise take exponential time. Past the budget the document is answered `ok: false` with a
`maxSteps` violation; it is never reported valid.

| Code | When |
|---|---|
| `F0002` | `document` is missing, `interface` is not a string or names no interface, `schema` cannot be compiled (an unsupported keyword such as `unevaluatedProperties`, a remote `$ref`, a bad pattern), or `package_dir` is not a string, is not a folder, is too long for the file system, cannot be read or comes with another interface. A `package_dir` that breaks the request-path rule also carries `data.reason` `path_outside_project` and `data.path_reason` |
| `F0003` | `package_dir` does not exist, or a folder on the way is a file. A path past the whole-path limit with an absent folder on the way answers this on every host (ADR-5550) |

The refusal reasons inside a result:

| Reason | Code | Meaning |
|---|---|---|
| `contract_too_large` | F0002 | over the byte limit (64 MiB; a kit 32,768 facet planes) |
| `contract_not_json` | F0002 | not UTF-8 JSON |
| `contract_no_schema_field` | F0002 | no `schema` string of the form `jewelry.<name>/<major>` |
| `contract_unknown_interface` | F0002 | an interface this engine does not know |
| `contract_wrong_interface` | F0002 | not the interface asked for |
| `contract_unsupported_major` | F0002 | a major this engine does not read |
| `contract_no_schema` | F0003 | a kit builder interface and no `schema` given |
| `contract_missing_field` | F0002 | a required field is absent |
| `contract_unknown_field` | F0002 | a field the schema does not define (cost and client data never pass) |
| `contract_wrong_type` | F0002 | a value of the wrong JSON type |
| `contract_bad_value` | F0002 | outside its enum or constant |
| `contract_out_of_range` | F0002 | a number outside its bounds |
| `contract_bad_string` | F0002 | a pattern or length (keys, hashes, paths, dates) |
| `contract_bad_count` | F0002 | too few or too many entries (also a convex cut with more than 512 facets: a finer shape is a mesh) |
| `contract_schema_mismatch` | F0002 | matches none of the allowed shapes |
| `contract_too_deep` | F0002 | nests deeper than 64 levels, or the schema needs more evaluations than its budget (1,000,000 plus 256 per value) |
| `contract_duplicate_id` | F0002 | an id, name or file appears twice |
| `contract_duplicate_facet` | F0002 | two materials claim one option label |
| `contract_unknown_reference` | F0003 | a reference to an id the file does not define |
| `contract_wrong_kind` | F0002 | a reference to an entry of the wrong kind |
| `contract_bad_range` | F0002 | min above max, default or step out of range |
| `contract_bad_transform` | F0002 | a quaternion or axis that is not unit length |
| `contract_bad_lods` | F0002 | LOD order |
| `contract_bad_normal` | F0002 | a facet normal that is not unit length |
| `contract_cut_no_facets` | F0002 | neither facets nor a mesh |
| `contract_cut_unbounded` | F0002 | a convex cut's facets do not close |
| `contract_cut_empty` | F0002 | a convex cut's facets leave nothing |
| `contract_cut_redundant_facet` | F0002 | a facet with no face |
| `contract_cut_size_mismatch` | F0002 | the solid's size differs from `size_mm` |
| `contract_cut_proportions_mismatch` | F0002 | crown height or pavilion depth differs from `proportions` |
| `contract_bad_spectrum` | F0002 | spectral arrays that disagree |
| `contract_hash_mismatch` | F0002 | `build_hash` or `package_hash` does not match |
| `contract_key_mismatch` | F0002 | a key of another house or design |
| `contract_house_not_allowed` | F0002 | a house the tenant may not load |
| `contract_inconsistent_resolution` | F0002 | resolution fields that disagree, or a build's `sku` or `house_variant_id` that differs |
| `contract_package_incomplete` | F0002 | no recipe, options, materials or mesh file, or more than one of the first three |
| `contract_package_size_mismatch` | F0002 | `sizes.total` is not the sum of the files |
| `contract_package_file_missing` | F0003 | a listed file is absent (`package_dir` only) |
| `contract_package_file_mismatch` | F0002 | a listed file's size or BLAKE3 hash differs (`package_dir` only) |
| `contract_package_has_cost` | F0002 | a JSON file holds a cost, price or markup field (`package_dir` only) |

## From the command line and in tests

`regia upgrade-project` lists every `*.json` file of these interfaces with its major and refuses a newer one;
no migrator exists yet (every format is at version 1). `cargo xtask check-formats` records each schema, so a
schema edit without a version bump fails the gate.

The contract tests in `crates/regia_configurator/tests/exchange.rs` validate every file Regia reads against the
`schema.json` its owner published under `$JEWELRY_OUT/exchange/`, and the real artifacts under `$JEWELRY_OUT`;
`REGIA_JW_CONTRACT_REQUIRED=1` turns every missing input into a failure (ADR-278). See the contracts page for the
exact layout.
