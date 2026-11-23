# Audio cook: keep or transcode to Ogg Vorbis (ADR-3230)

By default a project ships its sounds as the source files. `audio_cook.toml` at the project root, and the `.meta`
settings of a sound, can ship WAV and FLAC masters as Ogg Vorbis instead. Nothing changes in game code: the
sound keeps its path, its asset id and its loudness gain.

## The project table

```toml
mode = "vorbis"       # "keep" (default) or "vorbis"
quality = 0.4         # desktop Vorbis quality, -0.1 ..= 1.0 (about 128 kbit/s stereo at 0.4)
android_quality = 0.3 # Android quality (about 96 kbit/s stereo)
force = false         # also transcode .ogg and .mp3 sources (a second lossy generation)
```

Unknown keys, a malformed file or a quality out of range give a cook warning and the defaults.

## Per sound

The `.meta` `settings` string (RON), beside the loudness keys:

```ron
(mode: Some(keep), quality: Some(0.6), android_quality: Some(0.2), force: Some(true))
```

A per-sound key beats the project table. `.ogg` and `.mp3` sources are kept unless `force` is on.

## What happens

- The loudness record is measured on the **source**, so `keep` and `vorbis` give the same gain.
- The cook encodes into `<cooked cache>/audio/` (key: source bytes, quality, channels, encoder version). Two
  encodes of one input are byte-identical. Changing the policy recooks only the sounds it affects.
- `regia package` writes the Ogg bytes into `assets/` (and the APK) under the sound's own name. The runtime
  decoder reads the container by content, so a `door.wav` holding Ogg Vorbis plays as before.
- A sound whose channels are identical is encoded as one channel; the sample rate is kept.

## Reports

`regia cook` prints one line per sound (`audio assets/sfx/door.wav: vorbis q0.40, 441044 -> 38211 bytes`) and a
total. The same list is `audio_ship.json` in the cooked cache and `CookReport::audio_ship`.

## Errors

A source that does not decode fails the cook with the existing `audio import error` (the same text as before).
There is no new agent-protocol method: the audio policy is a cook-time setting, and the running game sees only
the shipped files.
