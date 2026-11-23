# Atlas page relocation regression recipe

This is a private CPU batch primitive check, not a new agent method. Run the focused oracle with:

```text
cargo test -p regia_2d --lib atlas_page_relocation_preserves_sprite_geometry_and_source_texels
cargo test -p regia_2d --lib missing_page_does_not_poison_the_next_valid_sprite_batch
```

The fixture describes the same trim, one-pixel-wide sprite, and untrimmed nine-slice on page zero and page one. Page dimensions and packed rectangle origins differ. It compares emitted positions, edge vectors, flip flags, instance counts and batch ranges exactly. An independent check maps UV endpoints to page pixels, subtracts the packed rectangle origin, and compares against the expected local source texel intervals (including each nine-slice cell) with a `1e-5` tolerance for `f32` round-trip error. Surrounding page bytes differ while logical sprite texels match.

The internal `Frame2d::push_sprite` call has no agent method. For the related read-only protocol surface, use `sprite.list` to inspect atlas, region, pivot and nine-slice data, then `sprite.get` for one entity. The existing `sprite2d::tests::sprites_and_tiles_are_queryable` agent unit test exercises both the successful `sprite.get` call and its missing-Sprite error. That error uses JSON-RPC `not_found` with the existing `F0003` data code; malformed entity parameters use `invalid_params` / `F0002`. These calls report authored metadata, not the internal batch's UV mapping.
