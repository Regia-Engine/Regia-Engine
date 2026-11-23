# Regia patches to lewton 0.10.2 (MIT OR Apache-2.0, upstream RustAudio/lewton)

Portions ported from lewton (https://github.com/RustAudio/lewton), version 0.10.2.
Copyright (c) 2016 est31 and contributors. MIT OR Apache-2.0; used under MIT.
The complete vendored source distribution is under `vendor/lewton/`; original copyright
notices remain in its source files and the full licence texts remain in `LICENSE`.

Upstream 0.10.2 panics on crafted Vorbis headers/packets; the shipping profile is `panic = "abort"`.
Changes (each marked `Regia patch` in the source):

1. `header.rs` floor 0: book index `>= codebook_cnt` rejected (was `>`), so `codebooks[idx]` is in range.
2. `header.rs` floor 0: `floor0_order < 2` rejected (curve computation underflowed).
3. `header.rs` codebook: `codebook_dimensions == 0` rejected (residue type 0 divided by it); the
   residue divide also uses `.max(1)`.
4. `audio.rs` residue type 2: `blocksize * channels` that does not fit `u16` is a decode error
   (the multiply wrapped); the caller checks the residue vector length before slicing.
5. `header.rs` comment header: the vendor length, comment count and each comment length are refused when
   the packet cannot hold them (they sized allocations first; a 7 KB WebM asked for 16 GB). ADR-2723.
6. `header.rs` codebooks: one setup header may ask for at most `SETUP_ELEMENT_BUDGET` (2^23) codebook
   entries plus vector scalars, and a lookup's multiplicand count must fit the bits left in the packet
   (`bitpacking.rs` gains `bits_left`). A few header bits described terabytes. ADR-2723.
7. `header.rs` ordered codebooks: a codeword length past 32 is refused. Empty runs raised the `u8` length past its end,
   a panic with overflow checks on and a wrap without. ADR-2723.

The upstream licence texts are in `LICENSE`. Entries in `docs/decisions.md`: ADR-2164, ADR-2723.
