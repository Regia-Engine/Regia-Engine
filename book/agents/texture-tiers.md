# Texture tier packages

Use `regia cook --texture-tier LOW=1024 --texture-tier HIGH=2048 --texture-tier ULTRA=0`
to produce smaller texture downloads without another texture encode (ADR-8059). This is a CLI
cook option; runtime inspection uses the existing agent methods below. No protocol schema changes.

Each tier is a complete directory under `build/paks/<platform>/texture_tiers/<NAME>/`, using the
same chunk names, asset IDs and existing content formats as the ordinary output. `textures.fpak`
in LOW contains reduced textures; a small texture's cooked bytes are unchanged. `ULTRA=0` copies
every original pak byte for byte, and an omitted flag changes no ordinary output. Registry records keep their tags/dependencies and report reduced cooked sizes. Reduced tier manifests
have distinct build IDs and updated file/resume-block hashes; `ULTRA=0` preserves the ordinary manifest too. Only `CookedTexture` entries are
reduced when a cooked standard material references them from a capped base color, normal,
ORM, emissive or packed detail albedo/normal slot (ADR-8144). References resolve across chunks,
by asset id or manifest path. Unreferenced textures and other material slots remain full size.
UI usage, HDR/float textures, cubes/arrays/volumes, terrain VT tiles and tails, HLOD atlases,
and voxel assets always keep their full cooked bytes, even if a material references them.
Raw images and embedded terrain/material/lightmap data stay as cooked. Author UI textures with
`usage: Some(ui)` in their import settings; a color texture shared with a capped material is
otherwise eligible. No new per-texture setting or source-format migration is required.

## Verification recipe

1. Cook the source once, then run the same cook with the tier flags. Warm-cache inputs stay
   skipped; the tier step copies mip payloads. The full cached chain remains intact.
2. Select LOW's directory as the host's content directory. Keep its manifest, maps and registry
   siblings together. For a downloadable chunk, use `content.set_source` and `content.request`
   for its logical name, then inspect `content.progress` until it is installed/mounted. The
   source must serve that tier's manifest build ID. Do not install full and reduced alternatives
   together. Normal `regia package` selects full output; tier staging is the host's choice.
3. Read `content.chunks` / `content.chunk`: names and asset counts are unchanged, while texture
   chunk sizes and hashes reflect LOW. `content.why` still reports the original assignment rule.
4. Have the host load the texture with `AssetServer::load_texture`, then call
   `assets.status {"asset": "assets/large.png"}` and `assets.cooked_pack {"inspect": true}`.
   Expect `state == "loaded"`, `errors == 0`, a texture pak load and `loads.cache_loads == 0`.
   The automated CLI test performs these inspection calls through the real agent transport
   against a game directory with no sources or cooked cache. The existing `assets.load`
   command and ID-based status lookup require a development cooked manifest, so use the host's
   loader and path-based status for pak-only tests.
5. In a rendered host, `assets.texture_memory` reports resident texture sizes. The runtime cap
   still applies, including when `.meta` disables streaming; it cannot recover discarded mips.

`--lightmap-tiers` can be combined with texture tiers, but remains a separate derivation in
`lightmap_tiers/<tier>/` in the cooked cache plus `lightmap_tiers.json`. It does not pack those
lightmap variants into these directories. Regenerate every tier you intend to publish: omitted
old tier directories remain on disk. Each requested directory replaces its old version only
after it has been completely written; a failure leaves that tier's previous version intact.

## CLI failures and warnings

| Code | Cause and action |
|---|---|
| `texture_tier_invalid` | Use `NAME=MAX_EDGE_PX` with an unsigned 32-bit cap, `0` for full resolution. Names: ASCII letter followed by letters/digits/`_`/`-`, up to 40 characters, excluding Windows device names. The flag cannot accompany a cache action. |
| `texture_tier_duplicate` | A name appears twice, ignoring case. Request each tier once. |
| `texture_tier_missing_mip` | Warning: no authored mip meets the edge cap. The entire cooked texture stays byte-identical, and other textures still reduce. Warns once per asset id per cook, across requested tiers and duplicate chunks. Supply a longer source chain to reduce that asset; no strict flag is needed. |
| `texture_tier_supercompressed` | A texture needing reduction has BasisLZ/Zstd/ZLIB or no concrete format. Recook its source to a runtime-supported format. |
| `texture_tier_ktx2` | Invalid container, dimensions, DFD or mip payload. Restore or recook the cached asset. |
| `texture_tier_manifest` | Ordinary paks are missing or differ from their manifest. Finish a successful ordinary cook. |

An eligible texture can therefore exceed the requested cap when its authored chain is too short.
Invalid containers and unsupported supercompression remain errors; they are never treated as a
missing-tail warning. I/O failures include the tier and underlying error. CLI errors exit nonzero; these are CLI
messages, not new RPC error codes. Existing agent failures keep their documented codes in
[cooked packs](cooked-packs.md) and [content chunks](content-chunks.md).

Tests: `cargo test -p regia_asset_pipeline --lib texture_tiers` and
`cargo test -p regia_cli --test texture_tiers`. They cover a 4096 BC7 chain at 1024/2048,
retained mip byte equality, runtime loading, small/full/default byte identity, metadata/alignment,
NPOT/array/cube/block layouts, malformed inputs, manifests, failed-write preservation and agent
inspection. No golden images or GPU card are needed.

The mixed-consumer regression also verifies byte identity for UI, HDR, cubes, HLOD,
voxel-far, terrain VT and unreferenced textures, beside a reduced 4096 material map.
`cargo test -p regia_cli --test texture_tier_missing_tail` packages a parsed one-level 2048 DDS
beside a 4096 BC7 chain and verifies byte identity, retained mip bytes and one warning across
two tiers and duplicate chunks. It packages authored levels directly: the default importer
rebuilds a single ordinary DDS level into a full chain, while multi-level DDS imports can
retain an incomplete authored chain.
