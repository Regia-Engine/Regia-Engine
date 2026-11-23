# Codec fixtures (ADR-1869)

Real-encoder WebM clips for `tests/codecs.rs`, `tests/fuzz.rs` and the `cargo fuzz` seed corpus. Each is
encoded from a synthetic source (no third-party content) by an offline encoder kept out of the repository
(spec 50 § 8). They were made on 2026-10-30 with ffmpeg 7.1 (libaom-av1, libopus, libvorbis):

| File | Bytes | Content |
|---|---|---|
| `av1_opus_64x48.webm` | 3909 | AV1 8-bit 4:2:0, 64x48, 24 fps, 12 frames, keyframes at 0 and 6 (hidden alt-ref frames and 3-byte show-existing-frame packets between); Opus mono 48 kHz, 1 kHz sine, 0.5 s, pre-skip 312 |
| `av1_10bit_65x33.webm` | 1514 | AV1 10-bit 4:2:0, 65x33 (odd sizes), 30 fps, 4 frames |
| `av1_vorbis_32x32.webm` | 7525 | AV1 8-bit 4:2:0, 32x32, 12 fps, 6 frames; Vorbis stereo 44.1 kHz (440 Hz left, 880 Hz right), 0.5 s |
| `av1_444_48x32.webm` | 1401 | AV1 8-bit 4:4:4, 48x32, 30 fps, 4 frames |
| `av1_422_10bit_48x32.webm` | 1372 | AV1 10-bit 4:2:2 |
| `av1_12bit_48x32.webm` | 1314 | AV1 12-bit 4:2:0 |
| `av1_gray_48x32.webm` | 1221 | AV1 8-bit monochrome |
| `av1_grain_48x32.webm` | 1773 | AV1 8-bit 4:2:0 with film grain (differs from the same stream decoded with grain off) |

```sh
ffmpeg -f lavfi -i "nullsrc=s=64x48:r=24,format=yuv420p,geq=lum='mod(X*4+N*16+Y*2\,256)':cb='mod(128+N*8\,256)':cr='mod(96+X\,256)'" \
  -f lavfi -i sine=frequency=1000:sample_rate=48000 -t 0.5 -frames:v 12 \
  -c:v libaom-av1 -cpu-used 8 -crf 45 -g 6 -c:a libopus -b:a 24k -ac 1 -f webm av1_opus_64x48.webm
ffmpeg -f lavfi -i "testsrc2=size=66x34:rate=30,format=yuv444p,crop=65:33:0:0,scale=65:33,format=yuv420p10le" \
  -frames:v 4 -c:v libaom-av1 -cpu-used 8 -crf 40 -g 4 -f webm av1_10bit_65x33.webm
ffmpeg -f lavfi -i testsrc2=size=32x32:rate=12 -f lavfi -i sine=frequency=440:sample_rate=44100 \
  -f lavfi -i sine=frequency=880:sample_rate=44100 -filter_complex "[1:a][2:a]amerge=inputs=2[a]" \
  -map 0:v -map "[a]" -t 0.5 -frames:v 6 -c:v libaom-av1 -cpu-used 8 -crf 50 -g 6 -c:a libvorbis -q:a 2 \
  -f webm av1_vorbis_32x32.webm
# the layout clips: pixel format and encoder arguments per file
for spec in "444 yuv444p" "422_10bit yuv422p10le" "12bit yuv420p12le" "gray gray" \
            "grain yuv420p -aom-params film-grain-test=1"; do
  set -- $spec; name=$1; fmt=$2; shift 2
  ffmpeg -f lavfi -i "testsrc2=size=48x32:rate=30,format=$fmt" -frames:v 4 -c:v libaom-av1 -cpu-used 8 -g 4 \
    -f webm -crf 45 "$@" av1_${name}_48x32.webm
done
```

Expected values in `tests/codecs.rs`:

- Picture hashes: FNV-1a 64 over Y, U, V (tightly packed; 16-bit little-endian above 8 bits) of every
  frame decoded by two independent AV1 decoders (libdav1d and libaom through ffmpeg,
  `-fps_mode passthrough -f rawvideo`), which agreed byte for byte.
- Opus: sample values and 960-sample block RMS of the libopus decode (`-c:a libopus -f f32le`, pre-skip
  and end padding trimmed by ffmpeg; the test compares the start only).
- Vorbis: sample values and the total count of the libvorbis decode (`-c:a libvorbis -f f32le`), which
  matched ffmpeg's own Vorbis decoder within 3.5e-5.
