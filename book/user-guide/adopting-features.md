# Adopting engine features

This page is for a game that already ships its own code for something Regia Engine now does. Each row
says what to adopt and what to drop: the host's own code, the engine feature that replaces it, how to
turn that feature on, the check that proves it works, and the open request that blocks the row. Direct
Order 27 (DO27) is the worked example at the end: its phased plan, row by row.

The names on this page were checked against the code on the v0.5.0 line on 2026-11-16, name by name.
Field-by-field host changes (struct literals, new fields, migrations) are in the
[DO27 host checklist](do27-host-checklist.md); the data DO27 and the engine exchange are in the
[DO27 interface contracts](do27-interface-contract.md). This page does not restate either.

## How to adopt a row

1. Turn the engine feature on next to the host's own code, behind a switch.
2. Run the row's check with the engine feature on and with the host's code on, on the same scene.
3. Delete the host's code in the same change that makes the engine feature the default. Keeping both
   usually does the work twice (two exposure clamps, two GI boosts, two Steam clients).
4. Keep the check in the game's own CI.

A row whose last column names an open request is not ready to replace the host's code in full. The row
says what works today and what the request is waiting for. Requests are named by their DO27 request name
and, where the engine has one, the engine lane that answers it. "None" means the row can be adopted now.

## Look and exposure

| Host's own code | Engine feature | Turn it on | Check | Open request |
|---|---|---|---|---|
| Exposure under rock: the host clamps or ramps the eye when the camera enters a tunnel or goes under rock | `ExposureZone` (`regia_render::exposure_zones`), weighed at the active camera every frame by `ExposureZonesPlugin` | Nothing to wire with `DefaultPlugins`: spawn one `ExposureZone` per bore with a `Transform` (`ExposureZoneShape::Box`, `ev_min`, `ev_max`, `blend_distance`, `priority`). A host without `DefaultPlugins` adds `regia_framework::exposure_zones::ExposureZonesPlugin`, or calls `set_exposure_target` and `set_exposure_dt` on its `FrameRenderer` before `render_into` | `render.get_exposure` in the tube (`mode` is `zone`, `zone_count`, `settled`); `render.exposure_log` over a drive; the wait atom `exposure_settled`. Tests `the_target_is_set_and_cleared_as_the_camera_crosses`, `the_host_step_reaches_the_renderer_eye` | None |
| Local exposure left off: shade on the hull's dark side lifted by a grade or a fill light | Local exposure: `LevelLocalExposure` on `LevelLook::local_exposure` (the level file) and `LocalExposure` on `PostSettings::local_exposure` (`shadow_contrast`, `highlight_contrast`, `detail_strength`); off by default | In the level file's `look`: `local_exposure: Some((shadow_contrast: 0.75))` (scene format v9). Live: `level.set_look` with `{"look": {"local_exposure": {"shadow_contrast": 0.75}}}`. A host that builds `PostSettings` itself calls `apply_level_look`. In the look-dev harness: `LOOK_R2_LOCAL_EXPOSURE=0.75` | `level.get_look`, then `seeing.screenshot` beside the reference plate. Tests `local_exposure_lifts_shadows_and_keeps_highlights_and_detail`, `a_level_look_turns_local_exposure_on_clamped` | None for the switch. A signed-off plate with it on waits for `look-europa-plate` (the owner's look round; no engine lane) |
| A clamp on the final exposure so a full-snow frame stays inside the map's band | The meter's band and the snow bias: `ExposureMeter::bright_bias_ev` on `PostSettings::meter`, from the level's `LevelMeter` | Set the band and `bright_bias_ev` in the level's look ([DO27 host checklist](do27-host-checklist.md), Europa grade) | Test `bright_compensation_scales_with_coverage` | `look-snow-bias-band`, lane `do27-exposure-band-bias`. Today the meter clamps to the band and then subtracts the bias, so a full-snow frame can land 1 EV under the floor. Keep the host's clamp until the lane lands |
| A fill lift for hull shade that must not lift every canyon | `TerrainSharedDraw::set_fill_canyon_visibility` (one switch for the whole map) | Off by default ([Host look checklist](host-look-checklist.md)) | `seeing.screenshot` with the switch on and off | `look-fill-canyon-scoped`, lane `do27-fill-canyon-scope`. On, the switch lifts every fill-lit shadow on the map, not only the shade under hulls |

## GI

| Host's own code | Engine feature | Turn it on | Check | Open request |
|---|---|---|---|---|
| A GI exposure boost: indirect light or exposure raised by hand so shade is not black under the Balanced option | The Depth lighting option: GPU irradiance probes (`GiLook::Depth`) that bounce the sun and sky every frame, seeded with the map's colors by `TerrainSharedDraw::bake_gi_albedo_map` | `RenderSettings::gi_look` left at `None` follows the preset (Depth on Desktop High and Ultra). The menu value `GameSettings.graphics.lighting` set to `LightingMode::Depth` turns it on for other desktop presets. Call `bake_gi_albedo_map` once at load. Live: `render.set_gi_mode` with `{"mode": "depth"}`. `GiLook::depth_available()` is false on Android. Drop the boost when you switch: the probes add the bounce the boost stood in for | `render.get_gi_mode` (`mode`, `setting`, `preset_default`); `render.get_dynamic_gi` (`settled_frames`); after `play.step` 30, `seeing.screenshot` with `balanced` and `depth` ([Lighting option](../agents/gi-probes.md)) | None inside the probes' reach. Reach ends at 324 m on Desktop Low and Deck and 549 m on High and Ultra ([Dynamic GI tiers](../agents/dynamic-gi.md)): `gi-open-terrain-reach`, lane `do27-gi-far-field-design` (design first) |
| A CPU GI field computed by the game and fed to the renderer | Dynamic GI (the Depth row above, no bake), or a baked volume per map (`.fgiv`) named by the level | Baked: `baked_gi: Some((volume: "maps/<map>.fgiv"))` in the level's `look` (scene format v10). `regia bake-gi` writes the volume; `DefaultPlugins` installs it when the level loads (otherwise add `BakedGiLevelPlugin`), so a host call to `set_baked_gi` can go | `level.get_baked_gi` (`source` is `level`, `error` is null). Test `a_level_installs_its_volume_and_unload_removes_it` ([Baked GI from the level](../agents/baked-gi-maps.md)) | Tunnel interiors: `tunnel-interior-gi`, lane `do27-tunnel-interior-gi` (interior shade reads the probe grid, not the baked volume). No DO27 map baked yet: `baked-gi-real-map` (an owner bake session; no engine lane). Open terrain past the cascade: `gi-open-terrain-reach`, lane `do27-gi-far-field-design`. Keep the CPU field only where one of these is still open |
| Shade under many units drawn by the host | Mesh occluders (`OcclusionProxy`, `FrameRenderer::set_mesh_occluders`, the 32 nearest) and GI occluders (`FrameRenderer::set_gi_occluders`, 64 capsules) | Add `OcclusionProxy` to a hull, or set the lists each frame; live: `render.set_gi_occluders` | `seeing.screenshot` with and without the proxies | `dynamic-unit-shade-caps`, lane `do27-occluder-far-band` (later). Past the caps nothing is drawn |

## Terrain and tunnels

| Host's own code | Engine feature | Turn it on | Check | Open request |
|---|---|---|---|---|
| Tunnel mouths cut by hand, or hidden under props | The terrain hole mask: `HolesMask` of `HoleShape` (`Rect`, `Polygon`, `Ellipse`, `Raster`, `HoleShape::segment` for a ramp), drawn by `TerrainSharedDraw::set_holes` and cut into the collider by `HolesMask::cut_heights` | Build the mask from the simulation's mouth list at load and call `set_holes`; or ship `<map>.holes.ron` and read it with `load_holes_ron` and `holes_ron_path`. Live: `terrain.set_holes`, `terrain.load_holes`. At most 64 shapes; one `Raster` counts as one ([Mesh tunnels](../agents/tunnels.md)) | `terrain.holes`; `terrain.raycast` down a mouth passes through. Test binary `do27_contract_examples` | `terrain-sim-grid`, lane `do27-terrain-grid-origin`: the engine's map has no XZ origin offset, so a simulation that samples cell centers is half a cell off ([Terrain runtime](../agents/terrain-runtime.md) has the spacing rule). Mouths larger than a cell are fine today |
| A tunnel tube modeled or built by the host | `MeshTunnel::from_collision`: the render tube from the simulation's collision triangles plus the mouth hole; `MeshTunnel::collision_trimesh` for the collider | Pass the collision positions and indices, a `HoleEllipse` mouth, `TunnelDetail` and a `CollisionProvenance` | Tests `mesh_tunnel_has_hole_and_trimesh`, `builds_from_collision_and_keeps_triangles` | None for one level of tunnels. Overhangs and stacked tunnels: `TUNNELS_FIX` terrain-mesh-layers-overhangs, lane `do27-terrain-mesh-layers-design` (design first) |
| Craters written into the host's own height copy | Runtime height patches: `TerrainHeightPatches` (`HeightPatch`, the `TerrainHeightsChanged` message), applied each frame under a sample budget | Insert `TerrainSharedDraw::height_patches_handle()` as the `TerrainHeightPatches` resource (`TerrainPlugin` applies the queue). Live: `terrain.patch_heights` with `col`, `row`, `width`, `height` and one of `heights`, `samples` (the simulation's 16-bit values) or `fill` | `terrain.get` until the patch id has completed. Tests `patch_heights_queues_and_the_system_applies_it`, `patched_heights_are_what_a_reload_reads_whatever_the_budget` | The grid origin, as in the first row (`terrain-sim-grid`). Deformation beyond heights (voxels): `terrain-voxel-core`, lane `do27-voxel-core-design` (later; build after DO27 signs the API) |

## Frame pacing and resolution

| Host's own code | Engine feature | Turn it on | Check | Open request |
|---|---|---|---|---|
| A frame governor that moves the render scale | `DynResController` (`regia_render`): a pure controller keyed on GPU time against the wall frame, so a CPU-bound frame never steps down | `DynResController::new(DynResConfig::default(), 1_000)`; each frame call `update` with `GpuTimings::frame_ms` and the present interval, and apply the returned permille to `FrameQuality::render_scale_permille` for that frame only. The engine does not wire it into the frame. Keep `ThermalGovernor` (heat) separate ([Dynamic resolution](dynamic-resolution.md)) | Tests `cpu_bound_scene_keeps_native_permille_for_three_minutes`, `gpu_bound_overload_steps_down_by_50_to_500_and_no_lower`; live: `permille`, `bound` and `median_gpu_ms` on the controller, `play.hitch_report` | None |
| Low with no upscaler choice in the menu | `TemporalUpscaler::Tsr`, the reconstruction upscaler, in place of the default `TemporalUpscaler::Taau` (Desktop Low renders at `render_scale` 0.75) | `RenderSettings.profile.temporal_upscaler` set to `TemporalUpscaler::Tsr` and `RenderSettings.profile.render_scale` (for example 0.67), or `FrameRenderer::set_temporal_upscaler`. Live, not persisted: `render.set_upscaler` with `{"backend": "tsr"}`. Caveat on the v0.5.x line: `RenderSettings::preset()` does not mask `temporal_upscaler` or `render_scale` yet, so with either changed it returns `None`, and systems that pick their tier from the preset (Low's ragdoll and vehicle-effects tiers) stop applying. Engine follow-up: lane `v051-preset-mask-upscaler` | `render.get_upscalers` (`current`, `temporal`); the nightly sweep's `low` profile runs `--tier mid --scale 0.67 --upscaler tsr` ([Nightly player sweep](nightly-sweep.md)) | None for the option. A floor below Low for integrated GPUs: `igpu-auto-quality` (lane `v051-auto-quality-minspec`) and `ultra-low-preset` (lane `do27-ultralow-design`, design first). Occlusion culling on Low: `masked-occlusion-low-tiers`, lane `do27-masked-occlusion-low` |

## Audio zones

| Host's own code | Engine feature | Turn it on | Check | Open request |
|---|---|---|---|---|
| Tunnel reverb and rock occlusion done by the host | `AudioReverbZone` with `ReverbZoneShape::Capsule` per tunnel section, `AudioReverbZone::open` for the surface, `ReverbPreset`; the shell supplies `AudioRockField` (ground height, `None` inside a hole) and `AudioZonePortals` (the mouths) | One `AudioReverbZone::tube` per section, rotated so the capsule's Y runs down the bore; `AudioReverbZone::mine_gallery` for galleries ([DO27 host checklist](do27-host-checklist.md)) | `audio.probe_zones` with the listener above, at the mouth and inside ([Audio zones](../agents/audio-zones.md)). Test `an_engine_in_a_tunnel_is_heard_from_the_surface_only_near_the_mouths` | None for zones. Which audio decoders a shipped build links is DO27's `PLATFORM_050` audio-decoders question, waiting for the owner (no lane) |

## Platform and Steam

| Host's own code | Engine feature | Turn it on | Check | Open request |
|---|---|---|---|---|
| The shell's own Steam init (`steamworks::Client::init`) and callback pump | `SteamPlugin` and the one shared client, `Steam::client()`; the engine runs the callbacks once per frame and shuts the client down on exit | The `regia` feature `steam`, plus `regia_steam/steamworks` for the real backend; the app id from `SteamSettings` or `[steam]` in `Regia.toml`. Match the `steamworks` version of `regia_steam` ([Steam](steam.md)). Never call `Client::init` next to the plugin | `steam.status`; the `SteamStatus` resource. Test `an_injected_backend_answers_the_restart_check_and_is_initialized_once` ([Steam (agents)](../agents/steam.md)) | None |
| Steam networking and lobbies | `regia_steam::SteamSocket` (a `DatagramSocket` for `NetServer` and `NetClient`) and `regia_steam::SteamLobbyPlugin` (backs `Lobbies`), both on the shared client | Add `SteamLobbyPlugin` after `SteamPlugin`; hand `SteamSocket` to the server and client in place of the UDP socket | `net.status`, `net.peers` ([Net lobbies (agents)](../agents/net-lobbies.md)) | None |

## Networking

| Host's own code | Engine feature | Turn it on | Check | Open request |
|---|---|---|---|---|
| The host's own transport, encryption and relay under a lockstep simulation | `NetServer` and `NetClient`, generic over the game's `NetGame` (`GAME_ID`, `PROTOCOL_VERSION`, `content_hash`) and a socket, with a Noise NX handshake by default; `TickDriver` hands the game each tick's inputs | Implement `NetGame`; step with `receive`, `poll_event`, `advance_tick` and `flush` on the game's own clock. `TickDriver::all_ready_for` (from `NetServer::driver`) holds a lockstep step until every player's input for that tick is in. Set `ClientConfig::server_key` to pin the host ([Networking](networking.md)) | `net.status`, `net.peers` on the host, `session.state` on a client; `cargo run -p regia_net --example net_duel`. Tests `driver_collects_in_peer_order_and_counts_misses`, `redundant_bundles_heal_a_lost_packet` ([Networking (agents)](../agents/net.md), [Net transport encryption](../agents/net-crypto.md)) | No world is needed, but the crate still links the engine's ECS crates: `net-core-without-ecs`, lane `do27-net-core-no-ecs-design` (design first). More than one relayed client per host: `net-relay-many-clients`, lane `do27-relay-many-design` (Steam sockets or an open port work today) |
| The host's own state replication | `SnapshotEncoder` with game-owned priority: a `WorldSnapshot` of `EntitySnapshot` against one `SnapshotSchema`, `EntitySnapshot::with_priority`, `encode_with_report` under a byte budget; what did not fit is in `EncodeReport::omitted`; the client reads with `SnapshotDecoder` | Build the snapshot from the simulation each send tick and set each entity's priority from the game's own relevance ([Snapshot delta codec](../systems/snapshot-delta.md)) | Tests `budget_packing_prefers_higher_priority_entities`, `budget_is_respected_and_omissions_are_reported` | None for the codec. Relevance and interpolation stay in the game by design (`net-external-sim-replication`, answered, no lane). Prediction of a game-owned step: `net-external-predicted-step`, lane `do27-net-rollback-log` (later) |

## Content and cooking

| Host's own code | Engine feature | Turn it on | Check | Open request |
|---|---|---|---|---|
| Decoding or importing source assets at load | Cooked packs: `regia cook --ship` stores desktop meshes GPU-ready, inspects every pak and fails on an entry that would be transcoded or cannot load, and writes `cooked_pack.json` | `regia cook --platform linux --ship` (and `windows`, `android`) ([Cooked packs](cooked-packs.md)) | `regia cook --pack-report` exits non-zero on an error finding. Test `ship_cook_loads_gpu_ready_meshes_and_bc_textures_from_the_pak_alone` | None with `--ship`. A player that cooks glTF at run time links the asset pipeline and its dependencies: `REGIA_PIN_050`, lane `do27-pipeline-runtime-features` |
| Hand migration of old project files after a pin bump | `regia upgrade-project` runs the content-format migrators: a dry run lists every file and whether it is current; `--write` migrates older files in place and keeps a `.bak` | Run it after every engine pin bump (`--engine` also moves the project's engine dependencies) | A second dry run lists every file as current | None |
| Older effect files, hard cuts between animations, the host's own accessibility layer | VFX format v7 (`VFX_FORMAT_VERSION`), `Inertializer` ([Inertialized transitions](inertialization.md)), the runtime UI's accessibility tree (`AccessibilitySettings`, the `accesskit` feature; [Accessibility](accessibility.md)) | Per page | `vfx.systems` and `vfx.describe`; `anim.get_state`; `ui.a11y_tree` | None |

## Telemetry

| Host's own code | Engine feature | Turn it on | Check | Open request |
|---|---|---|---|---|
| A frame log the game writes itself | `TelemetryPlugin` (in `DefaultPlugins`): session header, frame summaries, hitches, GPU errors; `regia_telemetry::json_layer` writes one JSON object per line | `TelemetryOptions::level` (`Normal` or `Verbose`), or `REGIA_TELEMETRY=off\|normal\|verbose` at startup ([Telemetry](telemetry.md)) | `telemetry.get_last_summary`, `telemetry.live` ([Telemetry (agents)](../agents/telemetry.md)). Test `a_thousand_frames_allocate_nothing_and_stay_under_budget` | Uploading logs from tester or player builds: `telemetry-privacy-allowlist`, lane `do27-telemetry-ship-level`. The session header still carries the adapter name, monitors and the settings dump; there is no allowlisted level yet |

## What stays in the game

On the v0.5.x line these stay in the game, and no row above replaces them. On the v0.6 line the engine takes
over the hover core, deterministic navigation and AI, lockstep host migration and input replays (`.lsreplay`) under
the DO27 absorb plan (v0.6 line); this page changes for v0.6 when those land.

- the simulation itself, and its interpolation of what the server sends;
- vehicle handling;
- AI and navigation for the game's own units (DO27 keeps its own);
- host migration: the engine does not move a session to a new host; reconnect grace resumes a dropped client on
  the same host only ([Networking](networking.md));
- input replays;
- the game's `NetGame` messages and inputs, and the relevance it writes as `SnapshotEncoder` priorities.

Debris and ragdolls next to an external simulation are not covered yet: `physics-presentation-world`, lane
`do27-presentation-physics` (later).

## Worked example: DO27's plan

DO27 builds on the v0.5.x line. Its plan has four phases; each item points at a row above.

### Phase 0: pin bump only

No engine feature is adopted. The steps are the pin bump, `regia upgrade-project`, DO27's own cook key check,
handling unchanged, the play gate and a DO27 Windows run. The engine's own Windows pass on v0.5.0 is DO27's
`windows-pass-release` request (answered: a lead and owner run, no lane).

### Phase 1: the next alpha

| DO27 item | Row | Adopt now? |
|---|---|---|
| Level look data with local exposure on | Look and exposure: local exposure | Yes |
| `ExposureZone` per tunnel, dropping exposure under rock | Look and exposure: exposure under rock | Yes |
| `AudioReverbZone` per tunnel | Audio zones | Yes |
| The render hole mask from the simulation's mouth list | Terrain and tunnels: mouths | Yes; exact grid agreement waits for `terrain-sim-grid` |
| The visible tube from the simulation's collision triangles (`MeshTunnel::from_collision`) | Terrain and tunnels: tube | Yes for one level of tunnels |
| `DynResController` in place of DO27's governor | Frame pacing: governor | Yes |
| A TSR option on Low | Frame pacing: TSR on Low | Yes, with that row's `preset()` caveat until `v051-preset-mask-upscaler` lands |

### Phase 2

| DO27 item | Row | Adopt now? |
|---|---|---|
| `regia_net` transport under lockstep: `NetServer` and `NetClient` with Noise and `TickDriver`, one protocol flag day | Networking: transport | Yes; the ECS-free build waits for `net-core-without-ecs` |
| The Steam client from `regia_steam` | Platform and Steam: init | Yes |
| Cooked packs (`regia cook --ship`) for load time | Content and cooking: cooked packs | Yes |
| Baked GI per map | GI: CPU GI field | After `baked-gi-real-map`; tunnel interiors after `tunnel-interior-gi` |
| The GI exposure boost and the CPU GI field dropped | GI: the first two rows | Inside the probes' reach; past it after `gi-open-terrain-reach` |
| VFX v7, inertialization and accessibility | Content and cooking: the last row | Yes |
| A telemetry subset | Telemetry | For local logs yes; uploads after `telemetry-privacy-allowlist` |

### Phase 3

| DO27 item | Row | Adopt now? |
|---|---|---|
| State replication on `SnapshotEncoder` with game-owned relevance and prediction | Networking: state replication | The codec yes; prediction of a game-owned step after `net-external-predicted-step` |
| Steam sockets and lobbies | Platform and Steam: networking and lobbies | Yes |
| Lockstep retired after a 40-bot soak | Networking | DO27's own soak decides; the engine's 32-40 client evidence is `net-scale-evidence` (no lane) |
| Craters through `terrain.patch_heights` | Terrain and tunnels: craters | Yes; exact grid agreement waits for `terrain-sim-grid` |
