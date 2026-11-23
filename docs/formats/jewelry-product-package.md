# Product package (`manifest.json`, `jewelry.product_package/1`)

Schema: [`jewelry-product-package.schema.json`](jewelry-product-package.schema.json). Shared rules and refusal
reasons: [jewelry-contracts.md](jewelry-contracts.md). ADR-2535. Registry name: `jewelry-product-package`. Format
version 1 (the major in `schema`); no migrator yet.

One product ready for a viewer: a folder of static files and this manifest. A web page fetches it over HTTP from any
static host; the Android viewer reads it from a local folder. There is no server, no stream and no signature. Path when
produced for the pilot: `$JEWELRY_OUT/packs/<house>/<product_slug>/`. The product viewer spec gives the layout and the
size budget (`docs/specs/42-product-viewer.md` § 8).

**Not in use yet:** today's packer, `regia viewer pack`, writes [`regia.viewer_package/1`](viewer-package.md) instead, and
the players read that. So this format is checked on fixtures only. ADR-2640 lists the differences and proposes moving the
packer here.

## Fields

Required: `format_version` (≥ 1, the version of the package layout and its cooked files, separate from the major in
`schema`); `package_version` (≥ 1, counts the rebuilds of this product's package); `regia_min_version` (`x.y.z`);
`product_key`; `design_key`; `house`; `snapshot_id`; `recipe { version, hash, design_key? }` (the source recipe; a build
copies it); `files[] { name, kind, size, blake3 }`; `package_hash`; `sizes { total, mesh?, data?, luts? }`.

Optional: `tenant_id` (`house:<slug>` or `shop:<slug>`, the business the package was built for, never a person);
`studio { studio_id, blake3 }` (the shared studio package this product needs).

`name` is one path component, relative to the manifest, with no leading `.`. `kind` is `mesh`, `recipe`, `options`,
`materials`, `kit`, `luts`, `thumb`, `texture` or `data`. A package holds exactly one `recipe`, `options` and
`materials` file, and at least one `mesh`. `blake3` values are 64 lowercase hex digits.

The JSON files inside hold what a viewer needs and nothing else. **The packer strips every cost, price and markup
field from the option data** (plan `interfaces.md` § 0.4): the viewer never prices anything.

## `package_hash`

The lowercase hex BLAKE3 of the RFC 8785 canonical JSON of the `files` array. It changes when any listed file changes,
so a host can use it as a cache key.

## Checks beyond the schema

On the manifest: `product_key` and `design_key` start with `house`, and the recipe is for the package's design
(`contract_key_mismatch`); a `house:<slug>` tenant holds only its own house (`contract_house_not_allowed`); file names
are unique (`contract_duplicate_id`); the required kinds are there (`contract_package_incomplete`); `sizes.total` is the
sum of `files[].size` (`contract_package_size_mismatch`); `package_hash` matches (`contract_hash_mismatch`).

On the files, `regia_configurator::checks::package_files` (the caller supplies a read function, so it runs over a
folder, a fetch or a map): each listed file exists (`contract_package_file_missing`) with its size and BLAKE3 hash
(`contract_package_file_mismatch`), and no `.json` file holds a field named `cost`, `dealer_cost`, `dealer_price`,
`markup`, `markup_rules`, `price`, `price_delta` or `retail`, in any letter case, at any depth
(`contract_package_has_cost`).

## Fixtures

`crates/regia_configurator/tests/fixtures/jewelry-product-package/package-basic.json` and its folder `package-basic/`:
a generic product with small stand-in mesh and table bytes, a stand-in recipe and option data, and a two-entry material
library.
