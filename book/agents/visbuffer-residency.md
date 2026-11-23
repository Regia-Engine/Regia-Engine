# Cluster residency

The visibility buffer (`book/agents/visbuffer.md`) keeps each mesh's cluster hierarchy on the GPU. Without
residency, every mesh on the path holds its whole hierarchy, so GPU memory grows with what the map has loaded.
Cluster residency (ADR-3118, spec 98 § 8c) sets a byte budget for this data. It keeps the coarse part of each
mesh resident and loads the finer parts only while the camera is close enough to need them. It is off by default.

## How it works

- Each mesh's clusters split into up to 16 **bands** (band 0 is the finest). The coarsest band is the
  **base**. The base is always resident, so a mesh never disappears.
- Each frame, every instance inside the last frame's view asks for the finest band its level-of-detail cut can
  use. The test is the same screen-error test the cluster cull uses. Residency loads the next finer band of each
  mesh that wants one, largest on screen first.
- When the budget is full, residency evicts the finest band of a mesh that no visible instance needs, least
  recently needed first. It never evicts a band a visible instance needs, and never a base.
- A band streams in over as many frames as the per-frame upload bound needs. It switches on only after its last
  byte is written, so a frame never draws a hole. While a finer band is missing, the finest resident band draws
  instead. The cut is still complete; it is coarser.
- The cluster cull does not change. Residency rewrites each cluster's error value so the finest resident band
  ends the hierarchy.

Limits:

- Meshes made resident before residency was turned on stay whole until their meshlets are inserted again or
  released.
- A managed mesh with more than one band cannot be a lightmap receiver on the path. Its vertices are not stored
  in one block, so the receiver draws forward.

## Disk streaming (ADR-3708)

When the cooked meshlets come from a pak (the normal shipped case), a managed mesh no longer holds its cluster
lists and triangles in RAM. The payload stays a view of the mapped pak behind a band disk, and residency reads
each band's lists and triangles on demand, on the engine's IO pool. A RAM budget bounds the delivered payloads:
over it, the least recently arrived payload that is not mid-upload is dropped back to the on-disk state and read
again when the band is next requested. An IO bound paces the reads per frame. The retained band plan (packed
vertices, cluster records) always stays in RAM and counts toward the RAM budget.

- The coarsest band (the base) is read at admission, before the first draw; finer bands arrive over the following
  frames. While a finer band loads, the mesh draws its coarsest resident level and the frame counts a fallback
  (`stats.fallbacks`). The base is always resident while the mesh is, so nothing disappears.
- Shadow cuts ask for their own bands (ADR-3708): a caster only a shadow job sees refines at the job's texel cut,
  at its own priority, without the main view seeing it.
- A failed read (`band_unreadable`, `band_corrupt`, `band_not_found`) leaves the band on disk: the mesh draws its
  coarsest resident level, the error is counted (`stats.read_errors`, `stats.last_read_error_code`), and the band
  is not retried until the mesh is inserted again. Nothing panics and no hole opens.
- The cooked `.fmlt` format is unchanged: the band plan is the index (offsets and sizes derive from the cluster
  records).

## Turning it on

In code: `RenderProfile::cluster_residency` (for example `ClusterResidency::for_preset(RenderProfilePreset::Deck)`),
or the runtime override `VisBufferSettings::residency` through `FrameRenderer::set_visbuffer_settings`.
Turning it on in the profile does not change `RenderSettings::preset()`. From an agent:

```json
{"method": "render.set_visbuffer_residency", "params": {"tier": "deck"}}
{"method": "render.set_visbuffer_residency", "params": {"budget_bytes": 268435456, "upload_bytes_per_frame": 4194304, "ram_budget_bytes": 268435456, "io_bytes_per_frame": 33554432}}
{"method": "render.set_visbuffer_residency", "params": {"follow_profile": true}}
```

`tier` takes a preset name and that preset's numbers, and turns residency on: `mobile_low` and `mobile_high` (128 MiB
VRAM, 2 MiB uploads, 128 MiB RAM, 16 MiB reads per frame), `deck` (256 MiB, 4 MiB, 256 MiB, 32 MiB), `desktop_low`
(512 MiB, 8 MiB, 384 MiB, 48 MiB), `desktop_high` and `desktop_ultra` (1 GiB, 16 MiB, 512 MiB, 64 MiB). These numbers
are targets until they are measured (master plan § 5.1a). After `tier`, the other parameters replace single fields:
`budget_bytes` bounds the cluster data on the GPU, `upload_bytes_per_frame` bounds the uploads, `ram_budget_bytes`
bounds the band payloads (and the retained plans) in RAM, `io_bytes_per_frame` bounds the pak reads.
`follow_profile: true` clears the override and must be passed alone. The call applies nothing unless every parameter
is valid. Changes apply from the next frame.

## Reading it

`render.get_visbuffer_residency {}` answers in any world. Fields are null when no renderer exists.

| Field | Meaning |
|---|---|
| `config` | The knobs in use: `enabled`, `budget_bytes`, `upload_bytes_per_frame`, `ram_budget_bytes`, `io_bytes_per_frame` |
| `profile`, `override`, `follows_profile` | Where the knobs come from |
| `stats.meshes`, `stats.unmanaged_meshes` | Managed meshes. Unmanaged meshes are whole: made resident while residency was off, or with only one band |
| `stats.resident_bytes`, `stats.peak_resident_bytes` | Bytes the managed meshes hold (records, resident bands, bands in flight). At or under `budget_bytes` while residency is on and the budget holds every band the view needs. It can be above it: a lowered budget stops evicting once every remaining band is needed, and after residency is turned off (`budget_bytes` 0) managed meshes keep what they hold |
| `stats.full_bytes` | What the managed meshes would hold with every band resident |
| `stats.bands` | Per band: managed meshes with that band resident, and meshes that have it |
| `stats.pending_bands`, `stats.refinements` | Bands loading now; bands switched on since start |
| `stats.evictions`, `stats.evicted_bytes` | Bands evicted since start |
| `stats.upload_bytes`, `stats.peak_upload_bytes`, `stats.upload_bytes_total` | Bytes uploaded last frame, at most in one frame, and in total |
| `stats.bound_overruns` | Frames over the bound. A band whose records alone exceed the bound uploads alone at the start of a frame. Raise the bound above 96 bytes × the largest band's clusters to keep this at 0 |
| `stats.base_upload_bytes` | New meshes' bases uploaded last frame. These are outside the bound |
| `stats.refused_meshes` | Meshes whose base did not fit the budget (or whose pak payload would not read). They draw forward; a budget refusal retries when room is made, a pak that would not read only when the mesh is inserted again |
| `stats.fallbacks` | Frames a visible mesh drew coarser than its demand: its finer band was loading, failed, or waited for budget. The coarsest resident level covered it |
| `stats.io_bytes`, `stats.io_bytes_total` | Band payload bytes delivered from the pak last frame and since start |
| `stats.read_queue_depth` | Band reads in flight (one per managed mesh at most) |
| `stats.bands_on_disk`, `stats.bands_loading`, `stats.bands_in_ram` | Per-band payload states: on the pak, a read in flight, delivered and awaiting upload |
| `stats.ram_bytes`, `stats.peak_ram_bytes` | Bytes the managed meshes hold in RAM (plans, delivered payloads, blocks in flight), and the high-water mark |
| `stats.ram_evictions` | Delivered payloads dropped back to the on-disk state under the RAM budget |
| `stats.read_errors`, `stats.last_read_error_code` | Failed band reads and the last code: `band_unreadable`, `band_corrupt`, `band_not_found` |
| `stats.degraded_meshes` | Meshes that wanted a finer band last frame than the budget allowed. A mesh larger than the whole budget stays at its base and is counted here |

Errors: `F0003` from `render.set_visbuffer_residency` when the app has no runtime renderer. A bad parameter gives
the protocol's invalid-parameter error. There are no codes of its own.

## Checking it

```text
render.set_visbuffer {"enabled": true}
render.set_visbuffer_residency {"tier": "deck"}
wait_until frames > 60
render.get_visbuffer_residency {}
expect stats.resident_bytes <= config.budget_bytes
expect stats.upload_bytes <= config.upload_bytes_per_frame
```

Fly the camera toward a dense mesh. `stats.refinements` rises and `stats.bands[0].resident` turns 1. Fly away and
fill the budget with other meshes. `stats.evictions` rises. `render.get_visbuffer` `creates.last_frame` stays at
0 objects once the cluster arena has grown.
