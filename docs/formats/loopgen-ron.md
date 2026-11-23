# Procedural loops: `.loopgen.ron` v1

A `.loopgen.ron` file describes a short looping sound as a small graph of generators and
filters. Regia Engine renders it to PCM in memory, so an `.audiopatch.ron` layer can play an engine
rumble, wind or machinery bed without a recorded file. Version 1; the runtime type is
`regia_audio::LoopGenDef` (`LoopGenDef::parse` / `from_ron`). Decision record: ADR-299.

## Determinism

Rendering is a **pure function of the file**: the same bytes give the same samples on every
platform (Linux, Windows, Android, macOS). The noise path uses integer random streams
(`regia_core::math::Rng`, PCG32), and oscillators, filters and gains use only `+ - * / floor
sqrt` plus the polynomial `sin`/`2^x` of the module, never the platform's `sin`/`exp`. Different
`seed` gives different noise; each channel and each node draws its own stream.

## Schema

```ron
(
    version: 1,                 // required, must be 1
    name: "machinery_hum",      // required, used in logs and the agent protocol
    sample_rate: 48000,         // 8000..=96000, default 48000
    length_secs: 2.0,           // required, 0.05..=60
    channels: 1,                // 1 or 2 (default 1); stereo draws independent noise per channel
    seed: 27,                   // default 0
    crossfade_secs: 0.15,       // wrap-around crossfade, 0..=length/2, default 0.1
    output_gain_db: -3.0,       // -96..=24, default 0; the result is hard-limited to +-1
    nodes: [ /* see below */ ], // 1..=64 nodes
    output: "throb",            // id of the node that is the loop
)
```

Each node is `(id: "<unique id>", node: <Kind>(...))`. Nodes refer to others by `id`; the graph
must not contain a cycle. Nodes the output does not depend on are validated but not rendered.

| Kind | Fields (default) | What it does |
|---|---|---|
| `Noise` | `color: White\|Pink\|Brown`, `gain` (1) | White is uniform in +-gain. Pink is Paul Kellet's filter (-3 dB/octave). Brown is leaky-integrated white (-6 dB/octave). |
| `Osc` | `shape: Sine\|Saw\|Square\|Triangle`, `hz`, `detune_cents` (0), `gain` (1), `phase` (0, in cycles) | Saw and square are naive (not band-limited): use low frequencies or follow them with a `Filter`. |
| `Rumble` | `cutoff_hz`, `q` (0.707), `gain` (1) | White noise through a low-pass, scaled by `sqrt(Nyquist / cutoff)` (at most 32) so a lower cutoff does not mean a quieter sound. |
| `Am` | `input`, `rate_hz` (0..=200), `depth` (0..=1) | `input x (1 - depth + depth x lfo)`, `lfo` a 0..1 sine starting at phase 0. |
| `Filter` | `input`, `mode: LowPass\|HighPass\|BandPass`, `cutoff_hz`, `q` (0.707) | RBJ biquad. BandPass has constant 0 dB peak gain. `cutoff_hz` 1..=0.98 x Nyquist, `q` 0.1..=20. |
| `OnePole` | `input`, `mode: LowPass\|HighPass`, `cutoff_hz` | 6 dB/octave. |
| `Gain` | `input`, `db` (-96..=48) | Gain in dB. |
| `Mix` | `inputs: [(input: "<id>", gain: 1)]` | Weighted sum. |

Gains of source nodes are limited to +-16 (linear). The full example is
`docs/formats/loopgen.example.ron`.

## Seamless loop

The graph is rendered `crossfade_secs` past the end of the loop. The first `crossfade_secs` of
the loop are an equal-power blend of the head with that continuation, so sample `N-1` flows into
sample `0` for any content (a 100.5 Hz sine in a 1 s loop included). The blend is normalised by
the measured correlation `r` of the two segments (gains divided by `sqrt(1 + 2·r·a·b)`, `r`
clamped to -0.5..1, ADR-872): unrelated content such as noise (`r` near 0) keeps plain
equal-power gains, and alike content such as a tone whose period divides the loop (`r` near 1)
does not swell by up to 3 dB in the middle of the wrap. With `crossfade_secs: 0`
the loop is cut as rendered, which is seamless only for content that is periodic in the loop
length. The seam is measured by `audio.loopgen_render`:

* `seam_jump`: `|first - last|` (largest over channels).
* `max_step`: the largest sample-to-sample step inside the loop.

The documented bound for a crossfaded loop is `seam_jump <= 1.1 x max_step + 0.0002`: the wrap
is no harder than the loudest step the loop already has. The tests check it for each generator.

The renderer also discards a 0.25 s warm-up so filters and integrators are settled when the loop
starts.

## Memory and caching

Rendered PCM is cached in memory by `LoopGenCache`, keyed by the hash of the file text
(FNV-1a 64 and the length): the same text renders once per cache. `SoundAssets` owns a cache;
registering the same file under two names renders it once. There is **no disk cache** in v1:
a render takes milliseconds, the result is a pure function of the file (so nothing can go
stale), and a disk cache would add invalidation and cleanup for no measured gain (ADR-299).
A loop is `length_secs x sample_rate x channels` 32-bit floats in memory (a 2 s mono loop at
48 kHz is 384 KB).

## Using a loop

* Patch layer: set a layer's `sound` (or a key's `sound`) to a path ending in `.loopgen.ron`
  and build the patch with `AudioPatch::from_def_with_loopgen`. The `.audiopatch.ron` format is
  unchanged (still version 1): `sound` was already a path.
* Code: `SoundAssets::add_loopgen(name, text)` / `add_loopgen_file(name, path)`, then play the
  name as any sound (emitters, cues). Loop generators are not hot-reloaded in v1.
* Agent: `audio.loopgen_render` and the `loopgens` field of `audio.list_emitters`
  ([agent guide](../../book/agents/audio.md)).

## Errors

Loading fails with the **key path** of the offending value, for example
`nodes[1].node.Filter.cutoff_hz: 90000 is outside 1..=23520`, `output: unknown node "x"`,
`nodes[0].id: "a" feeds itself (the graph has a cycle)`, or `version: 2 is newer than this
engine reads (1)`. A syntax error has the key path `(file)`.

## Versioning

Version 1 is the first version, so there is no migrator. A shape change needs a version bump,
a migrator and `regia upgrade-project` support (`docs/specs/12-assets.md` § 4a); `cargo xtask
check-formats` guards the shape (field names, enum variants and the node fields).
