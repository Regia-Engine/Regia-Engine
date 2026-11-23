# Cargo features

Every Cargo feature of the engine crates is listed, with what it does and its guide page, in § 1 of the
[coverage map](../COVERAGE.md); `cargo xtask book-coverage` fails when a crate gains a feature with no row there
([book coverage](../agents/book-coverage.md)). This page says which ones a game chooses.

## What a game turns on

A game depends on `regia`. Its default features (`agent`, `ui`, `quick`) suit development: the agent server, the
runtime UI and the one-file app helpers. Shipping builds turn the defaults off (`regia package` does it) unless the game
opts back in. The optional ones a game may add:

| Feature | When |
|---|---|
| `steam` | The game ships on Steam (`regia::steam`) |
| `crash` | The game uploads crash reports (`regia::crash`; add `regia_crash/tls` for `https://` endpoints) |
| `net` | The game uses client-server replication (`regia::net`) |
| `voice-net` | The game carries voice chat over `regia::net` (`regia::net::voice`, implies `net`) |
| `voice-opus` | Voice chat speaks Opus (`regia::media::voice_opus`, implies `media`) |
| `physics2d` | The game uses 2D bodies and colliders |
| `frame-gen` | FSR 3 frame generation: an interpolated frame between every two rendered ones, desktop only, off until the game or an agent enables it (`render.set_frame_gen`) |
| `dev-assets` | Dev runs only (`regia run --dev`): never in a shipped build |
| `static-dxc` | Windows builds that use the DX12 fallback with the statically linked DXC |
| `accesskit` | The game's UI talks to the player's screen reader (AT-SPI, UI Automation, NSAccessibility; [Accessibility](accessibility.md)) |

`regia_dylib` has the same features and forwards each to `regia`: the editor's hot-reload build links the engine
through it ([hot reload](hot-reload.md)).

## Internal features

Some features exist for the engine's own tests and tools and are never turned on by a game:

- `regia_render/test-hooks`: out-of-memory hooks (`texture_stream::inject_stream_texture_oom`,
  `inject_stream_upload_oom`, `gpu_health::release_texture_mip_hold`) that allocate a huge texture on purpose. The
  renderer's own tests turn it on through a dev-dependency.
- `regia_agent/reference-trace` and `denoise`, `regia_pt/denoise`: desktop tools only (the reference path tracer and
  the native OIDN denoiser).
- `regia_ecs/debug_names` and `regia_app/debug_names` (on by default): keep system and component names for the agent
  protocol, the profiler and `App::shuffle_schedules`; a shipping build may turn them off.
