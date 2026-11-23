<!-- Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063564), files
crates/bevy_anti_alias/src/smaa/SMAAAreaLUT.ktx2 and SMAASearchLUT.ktx2, Copyright (c) Bevy
contributors, MIT OR Apache-2.0, used under the MIT licence; the tables are SMAA's precomputed
textures (https://github.com/iryoku/smaa, Copyright (C) 2013 Jorge Jimenez, Jose I. Echevarria,
Belen Masia, Fernando Navarro and Diego Gutierrez, MIT licence). Both licence texts are in the
header of `../shaders/smaa.wgsl`. -->

# SMAA lookup tables

The two precomputed tables SMAA 1× reads in its blending-weight pass (`shaders/smaa.wgsl`,
`src/smaa.rs`). They are the level-0 texels of Bevy's KTX2 files with the zstd supercompression
removed (no other change), stored raw so the runtime needs no decoder:

| File | Format | Size | Bytes | SHA-256 |
|---|---|---|---|---|
| `smaa_area.rg8` | `Rg8Unorm`, rows top to bottom | 160 × 560 | 179 200 | `35065cef2a02cabcad711d6bf430239ae64e27d71c4e4fa06f29cce2c992f0d2` |
| `smaa_search.r8` | `R8Unorm`, rows top to bottom | 64 × 16 | 1 024 | `3694eae5e9d44b8ebb4415a13f8c7b94dc08a2fc86658434d771c4610fe5744d` |

How they were extracted: read the KTX2 level index (vkFormat 16 = `R8G8_UNORM` and 9 = `R8_UNORM`,
supercompression scheme 2 = zstd), slice level 0 and `zstd -d` it. The search table holds only the
values 0, 127 and 254, as SMAA's `SearchTex.h` does; `smaa::tests` checks sizes and that value set.
