# Specs index

Specs written from the public-sources spec set (ADR-1526, behavior only; ADR-1578, 1596, 1599 and later batches; every spec below was reviewed under ADR-1585, see `reviews/`). Builders read the Regia spec only.

| Spec | Plan item | Crates |
|---|---|---|
| [32 — Asset validation, reference graph and size map](32-asset-validation-and-audit.md) | editor asset audit | `regia_asset_pipeline`, `regia_cli`, `regia_editor`, `regia_agent` |
| [33 — Spatial queries for AI](33-spatial-queries.md) | AI | `regia_ai`, `regia_nav`, `regia_agent` |
| [34 — Montage and motion-warping extensions](34-montage-extensions.md) | animation | `regia_animation` |
| [35 — Gameplay camera framework](35-camera-framework.md) | gameplay | `regia_framework`, `regia_render`, `regia_agent` |
| [36 — Localization exchange, dialogue and subtitles](36-localization-exchange-dialogue-subtitles.md) | localization | `regia_ui`, `regia_asset`, `regia_cli`, `regia_framework` |
| [37 — One-file-per-entity levels, layers and placed levels](37-per-entity-levels-layers-instances.md) | G6 | `regia_scene`, `regia_asset`, `regia_cli` |
| [38 — State trees](38-state-trees.md) | G7 | `regia_ai`, `regia_agent` |
| [39 — Smart objects v2](39-smart-objects-v2.md) | G8 | `regia_ai`, `regia_agent` |
| [40 — Content chunks, DLC paks and Android asset packs](40-content-chunks-dlc-asset-packs.md) | G9 | `regia_asset`, `regia_asset_pipeline`, `regia_cli`, `regia_platform` |
| [41 — In-level functional tests and the multi-process session harness](41-functional-tests-and-session-harness.md) | G10 | `regia_framework`, `regia_agent`, `regia_cli`, `regia_test_utils` |
| [42 — Product viewer (web player and Android viewer)](42-product-viewer.md) | JW-L1 to JW-L6 (`master-plan.md` § 13; written from the plan, not the public-sources set; review pending) | `regia_configurator`, `regia_viewer`, `regia_web` (new), `samples/product_viewer` (new), `regia_cli`, `regia_agent` |
| [50 — Video playback](50-video-playback.md) | G11 | `regia_media` (new), `regia_render`, `regia_audio`, `regia_ui` |
| [51 — Curve tables and choosers](51-curve-tables-and-choosers.md) | data | `regia_core`, `regia_framework`, `regia_editor` |
| [52 — Motion matching](52-motion-matching.md) | animation | `regia_animation` |
| [53 — Abilities, attributes and effects](53-abilities-attributes-effects.md) | gameplay | `regia_framework`, `regia_ai` |
| [54 — GPU breadcrumbs and trace markers](54-gpu-breadcrumbs-and-trace-markers.md) | diagnostics | `regia_render`, `regia_telemetry`, `regia_crash` |
| [55 — Cloth, skinned cloth and soft bodies](55-cloth-and-soft-body.md) | physics | `regia_physics`, `regia_framework`, `regia_animation` |
| [56 — Dialogue, captions and subtitles](56-dialogue-and-subtitles.md) | narrative | `regia_ui`, `regia_audio`, `regia_cli` |
| [57 — Plugin and extension API](57-plugin-extension-api.md) | extensibility | `regia_editor`, `regia_app`, `regia_framework` |
| [58 — OpenXR](58-openxr.md) | XR | `regia_xr` (new), `regia_platform` |
| [59 — Voice chat](59-voice-chat.md) | audio/online | `regia_audio` |
| [60 — DCC scene feed](60-dcc-scene-feed.md) | tools | `regia_agent`, `regia_editor`, `regia_asset_pipeline`, `regia_cli` |
| [61 — Starter content, sample game and templates](61-starter-content-and-sample-game.md) | templates | `regia_templates`, `regia_ui`, `regia_ai`, `regia_framework` |
| [62 — Procedural content graph (core)](62-procedural-content-graph.md) | procedural content graph | `regia_procgen` (new), `regia_scene`, `regia_agent`, `regia_cli` |
| [63 — Procedural node library](63-procedural-node-library.md) | procedural content graph | `regia_procgen` |
| [64 — Splines, polygons, modules along paths and roads](64-splines-polygons-roads.md) | procedural content graph | `regia_scene` (spline), `regia_procgen`, `regia_terrain` (flatten), `regia_framework` (camera path code move) |
| [65 — Biomes and vegetation scatter](65-biomes-and-vegetation-scatter.md) | procedural content graph | `regia_procgen`, `regia_terrain` (grass multiplier), `regia_cli` (scatter import) |
| [66 — Tiled, multi-scale and runtime generation](66-tiled-and-runtime-generation.md) | procedural content graph | `regia_procgen` |
| [67 — Procedural graph editor and interactive tools](67-procedural-graph-editor.md) | procedural content graph | `regia_editor` |
| [68 — Procedural plants](68-procedural-plants.md) | procedural content graph | `regia_procgen` (pipeline feature), `regia_cli`, `regia_editor` |
| [70 — Network model: process kinds, peers, authority, ownership and net messages](70-net-model-authority-messages.md) | D2/D3 (`regia_net`, W-F) | `regia_net`, `regia_agent` |
| [71 — Entity replication](71-entity-replication.md) | D3 (`regia_net`, W-F) | `regia_net`, `regia_macros`, `regia_agent` |
| [72 — Snapshot delta, relevance and priority](72-snapshot-delta-relevance-priority.md) | D3 (`regia_net`, W-F) | `regia_net`, `regia_agent` |
| [73 — Client prediction and server reconciliation](73-client-prediction-reconciliation.md) | D3 (`regia_net`, W-F) | `regia_net`, `regia_framework`, `regia_agent` |
| [74 — Snapshot interpolation for proxies](74-snapshot-interpolation.md) | D3 (`regia_net`, W-F) | `regia_net`, `regia_agent` |
| [75 — Lag compensation for hit checks](75-lag-compensation.md) | D4 (`regia_net`, W-F) | `regia_net`, `regia_physics`, `regia_animation`, `regia_agent` |
| [76 — Dedicated server package](76-dedicated-server-package.md) | v0.3.x (b) headless server target, v0.5.0 server package (W-F) | `regia`, `regia_cli`, `regia_net`, `regia_agent` |
| [77 — Session lifecycle](77-session-lifecycle.md) | D1 sessions, D7 content handshake, T2.3 tools (W-F) | `regia_net`, `regia_framework`, `regia_editor`, `regia_agent` |
| [95 — Loudness at import, and the remaining texture importers](95-loudness-importers.md) | v0.5.0 importers, `specs/22` T2.14 | `regia_audio`, `regia_asset_pipeline`, `regia_agent`, `regia_editor` |
| [99 — Weather: precipitation, surface wetness and snow, weather-driven scene wind](99-weather-precipitation.md) | Gap audit 10-30 E5; `versions.md` v0.2.x-v0.3.x weather candidates (W-E, renderer phases in the W-A slot) | `regia_scene`, `regia_vfx`, `regia_render`, `regia_terrain`, `regia_asset`, `regia_agent` |
