# Terrain virtual texture tiers

The [terrain virtual texture](terrain-vt.md) turns on by itself only where it was measured: Desktop
Low, on Linux, on a discrete card with 6 GB or more that is not AMD. The quality detector's
terrain policy lets a game opt into more: Desktop High, Windows, and AMD cards. Every opt-in is off
by default, so the shipped presets render exactly as before.

## Methods

- `quality.get_terrain_vt_policy` reads the policy and what it decides:
  - `policy`: `{ high, windows, unmeasured_vendors }`, all `false` by default.
  - `excluded_vendors`: PCI vendors without a measurement (`["0x1002"]`, AMD).
  - `min_vram_mb.low` (6144) and `min_vram_mb.high` (11776): the card memory each tier needs.
  - `high_budget_bytes`: the most GPU memory the texture may take on Desktop High (768 MiB).
  - `os`, `render_tier`, `lever` (the profile's `terrain_budget.virtual_texture`).
  - `late_depth`: the profile's `terrain_budget.vt_late_depth`. The detector sets it with the
    lever only under `windows`, so a DX12 device takes the late-depth path only when asked.
  - `decides`: what the policy decides for the running GPU and tier (`null` before the render
    device exists).
- `quality.set_terrain_vt_policy { high?, windows?, unmeasured_vendors? }` changes the given
  opt-ins and keeps the others. It stores the policy for every later preset change and re-decides
  the lever on the running tier at once (`lever` in the result; `null` when there is no render
  device yet). A lever the host set against the old policy (on or off) keeps the host's value.
  The policy is not saved with the settings: a game sets it at startup.
- `terrain.vt` also reports `depth_test`:
  - `early`: forced early depth tests (Vulkan). Any settings run.
  - `late`: no forced early depth tests (DX12, Metal). Only settings with
    `late_depth_fallback: true` run. The render profile's lever runs them only with
    `terrain_budget.vt_late_depth` (set under `windows`); without it the terrain shades in full,
    as before.
  - `unsupported`: too few bind groups or buffer slots.
- `terrain.set_vt` accepts `late_depth_fallback` in `settings`.

## What each opt-in does

| Opt-in | Effect | Measured? |
|---|---|---|
| `high` | Desktop High also gets the texture, on discrete cards with at least 11.5 GB (a 12 GB card qualifies, a 10 GB one does not). Medium and Ultra never do. | Memory: yes (lavapipe, see below). Frame time: not yet. |
| `windows` | Windows builds qualify like Linux ones. Vulkan runs as on Linux. DX12 runs the late-depth path and reads the card's memory from DXGI. | Type-checked only. |
| `unmeasured_vendors` | AMD cards qualify. | Not on RADV yet. |

## Why AMD is excluded

The exclusion is a missing measurement, not a known fault. The RX 7900 could not be run on the
day the default shipped. A review of the texture's code found nothing AMD-specific:

- derivatives are taken in uniform control flow;
- atlas reads use explicit LODs;
- the feedback atomics are safe in helper lanes;
- no subgroup size is assumed;
- every format and limit used is far below what RADV offers.

On lavapipe with the Khronos validation layer the texture's tests report no validation message.
Run the same check on an AMD card before turning the opt-in on by default:

```text
VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation \
VK_KHRONOS_VALIDATION_DEBUG_ACTION=VK_DBG_LAYER_ACTION_LOG_MSG \
VK_KHRONOS_VALIDATION_LOG_FILENAME=/tmp/vt-validation.log \
cargo test -p regia_terrain --test terrain_vt --test terrain_vt_tiers -- --test-threads=1
grep -c "Validation Error" /tmp/vt-validation.log
```

All tests should pass, and the count should be 0.

## The late-depth path (DX12, Metal)

The texture's colour pass writes page requests from the fragment shader. With forced early depth
tests, only visible fragments run it. Without them, the depth test happens after the shader:

- **The frame is the same.** The test `terrain_vt_tiers` checks this against the uncached frame
  and the early-depth frame.
- **Hidden fragments still run the shader and request pages.** More pages become resident, and
  the hit rate also counts hidden fragments.
- **The cost is higher.** It has not been measured on a DX12 or Metal driver.

## Budget on Desktop High

High uses the same settings as Low. The atlas is 671 MB, plus a few MB of page tables and
buffers. `terrain_vt_tiers` draws Desktop High frames with the lever on and reads every
`terrain vt` allocation from the allocator report. The test checks two things:

- the total holds the atlas and stays within `high_budget_bytes`;
- no frame bakes more pages than `bakes_per_frame` and `rebakes_per_frame` allow.

## Errors

`quality.set_terrain_vt_policy` answers `F0002` (bad params) when:

- the params are not an object with at least one field;
- a field is not a boolean;
- a field is unknown (known: `high`, `windows`, `unmeasured_vendors`).

A refused call changes nothing.

## Example calls

Example calls (`regia agent call <method> '<params json>'`):

```text
quality.get_terrain_vt_policy {}
quality.set_terrain_vt_policy {"high": true}
terrain.vt {}
terrain.set_vt {"enabled": true, "settings": {"late_depth_fallback": true}}
```
