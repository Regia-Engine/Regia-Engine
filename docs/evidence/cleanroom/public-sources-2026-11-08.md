# Public-page citation inventory, 2026-11-08

ADR-4581; owner request 2026-11-08. The spec-writer pass read only the Sources/reference
sections of the 60 externally retained set files identified below. It transcribed URLs,
with no surrounding wording, titles, names or identifiers. The two additional public-source
sets were not needed: these 37 gap rows cite areas from the main set.

The 37 repaired log rows contain 1,805 URL occurrences after deduplication within each
spec, representing 1,323 distinct URLs globally. The matching spec provenance sections
repeat those same 1,805 citations. Sixteen implementation
rows that also cite these set areas received a direct public-page citation. Every source
area has a usable URL; no repository, source-file, header, shader or decompiled-material
URL was found in the extracted list, so none was refused. Public API-reference pages are
documentation citations under ADR-1526 and ADR-1661 (4). Names occurring inside their URL
hosts and paths are citations, not Regia terminology.

An independent extraction that preserves balanced URL parentheses returned the same lists
for all 60 Sources sections. A separate comparison confirmed that every extracted URL is
present in the corresponding log row and Regia spec provenance section.

The classification review distinguishes published standards from ordinary uses of the word
"standard": 67 primary labels were refined to reflect the existing source descriptions.
The color-conversion rows also retain their original public-domain claim and now cite the
author's [MIT-availability statement](https://bottosson.github.io/posts/oklab/). No code was
ported during this documentation pass.

## Counts by set source area

Counts are distinct URL strings in each Sources section, before cross-area deduplication.
The spec columns identify Regia documents; multiple rows can use the same source area.

| Set source area | Regia specs | URLs |
|---|---|---|
| `developer-insights-timing-analysis` | 54 | 40 |
| `developer-insights-trace-capture` | 54 | 19 |
| `developer-tools-localization` | 36, 56 | 26 |
| `editor-asset-editors-data` | 51 | 3 |
| `editor-content-asset-audit` | 32 | 25 |
| `editor-content-asset-management` | 32 | 23 |
| `editor-core-overview` | 61 | 11 |
| `editor-core-packaging` | 61 | 8 |
| `runtime-ai-environment-queries` | 33 | 26 |
| `runtime-animation-montages` | 34 | 12 |
| `runtime-animation-pose-search` | 52 | 6 |
| `runtime-audio-mixing` | 59 | 27 |
| `runtime-audio-overview` | 59 | 10 |
| `runtime-audio-spatial` | 59 | 15 |
| `runtime-characters-cloth-assets` | 55 | 7 |
| `runtime-characters-digitalhumans` | 60 | 50 |
| `runtime-core-modules-plugins` | 57 | 39 |
| `runtime-gameplay-abilities` | 53 | 31 |
| `runtime-gameplay-cameras` | 35 | 76 |
| `runtime-gameplay-choosers` | 51 | 35 |
| `runtime-gameplay-modular-features` | 57 | 57 |
| `runtime-gameplay-smart-objects` | 39 | 43 |
| `runtime-gameplay-state-trees` | 38 | 50 |
| `runtime-gameplay-tags` | 53 | 17 |
| `runtime-media-overview` | 50 | 44 |
| `runtime-media-playback` | 50 | 18 |
| `runtime-media-subtitles` | 36, 56 | 25 |
| `runtime-networking-actor-replication` | 71, 72 | 9 |
| `runtime-networking-movement-prediction` | 73, 74, 75 | 20 |
| `runtime-networking-netrep` | 71, 72 | 11 |
| `runtime-networking-overview` | 70, 71, 74, 75, 76, 77 | 15 |
| `runtime-networking-remote-procedure-calls` | 70 | 15 |
| `runtime-networking-replication-graph` | 72 | 7 |
| `runtime-networking-transport-debugging` | 76, 77 | 35 |
| `runtime-online-backends` | 59 | 12 |
| `runtime-online-overview` | 59 | 22 |
| `runtime-patching-chunk-downloading` | 40 | 4 |
| `runtime-patching-chunking` | 40 | 7 |
| `runtime-patching-install-bundles` | 40 | 51 |
| `runtime-physics-cloth-deformables` | 55 | 17 |
| `runtime-procedural-biomes` | 65, 66 | 6 |
| `runtime-procedural-data-attributes` | 62, 63, 64 | 66 |
| `runtime-procedural-graph-authoring` | 62, 64, 66, 67 | 17 |
| `runtime-procedural-node-reference` | 63, 64 | 26 |
| `runtime-procedural-overview` | 62, 63, 65, 66, 67 | 12 |
| `runtime-procedural-vegetation` | 68 | 1 |
| `runtime-rhi-gpu-diagnostics` | 54 | 8 |
| `runtime-sequencer-cameras` | 60 | 13 |
| `runtime-sequencer-playback` | 60 | 22 |
| `runtime-tests-functional` | 41 | 19 |
| `runtime-tests-session-harness` | 41 | 4 |
| `runtime-worldgrid-data-layers` | 37 | 17 |
| `runtime-worldgrid-external-actors` | 37 | 18 |
| `runtime-worldgrid-level-instances` | 37 | 14 |
| `runtime-xr-ar-sessions-and-tracking` | 58 | 62 |
| `runtime-xr-controllers-and-hand-tracking` | 58 | 35 |
| `runtime-xr-openxr` | 58 | 37 |
| `runtime-xr-overview` | 58 | 27 |
| `runtime-xr-virtual-reality` | 58 | 21 |
| `virtual-production-motion-link` | 60 | 18 |

## Counts by repaired Regia spec

Each count is the deduplicated union of that spec's source-area URLs, recorded in both
its provenance section and its shared log row.

| Regia spec | URLs added to gap row |
|---|---|
| 32 | 48 |
| 33 | 26 |
| 34 | 12 |
| 35 | 76 |
| 36 | 50 |
| 37 | 48 |
| 38 | 50 |
| 39 | 43 |
| 40 | 60 |
| 41 | 23 |
| 50 | 51 |
| 51 | 38 |
| 52 | 6 |
| 53 | 48 |
| 54 | 67 |
| 55 | 21 |
| 56 | 50 |
| 57 | 96 |
| 58 | 161 |
| 59 | 76 |
| 60 | 103 |
| 61 | 17 |
| 62 | 92 |
| 63 | 100 |
| 64 | 107 |
| 65 | 16 |
| 66 | 31 |
| 67 | 27 |
| 68 | 1 |
| 70 | 28 |
| 71 | 25 |
| 72 | 24 |
| 73 | 20 |
| 74 | 35 |
| 75 | 35 |
| 76 | 47 |
| 77 | 47 |

## Public-page spot checks

All 1,323 extracted URL strings were screened for repository hosts, source-file extensions
and decompiled-material paths. For each of the 60 source sections, the pass selected one
representative page, preferring a workflow page where present; it also sampled API-reference
pages from three areas. Shared representatives were fetched once: 62 distinct HTTP GETs,
62 successful HTTP 200 responses with HTML document metadata. Redirects to repository or
source-file material were refused before opening. No such redirect occurred. This is a
spot check, not a claim that every deep link was live or that HTTP 200 proves page content.
No page text was transcribed into Regia. Three representative pages were additionally
checked through the web reader.

| Representative citation | HTTP status | Public-page content type | Areas checked |
|---|---|---|---|
| <<public engine documentation>> | 200 | `text/html` | `developer-insights-timing-analysis` |
| <<public engine documentation>> | 200 | `text/html` | `developer-insights-trace-capture` |
| <<public engine documentation>> | 200 | `text/html` | `developer-tools-localization` |
| <<public engine documentation>> | 200 | `text/html` | `editor-asset-editors-data` |
| <<public engine documentation>> | 200 | `text/html` | `editor-content-asset-audit` |
| <<public engine documentation>> | 200 | `text/html` | `editor-content-asset-management` |
| <<public engine documentation>> | 200 | `text/html` | `editor-core-overview` |
| <<public engine documentation>> | 200 | `text/html` | `editor-core-packaging` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-ai-environment-queries` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-animation-montages` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-animation-pose-search` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-audio-mixing` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-audio-overview` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-audio-spatial` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-characters-cloth-assets` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-characters-digitalhumans` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-core-modules-plugins` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-gameplay-abilities` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-gameplay-cameras` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-gameplay-choosers` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-gameplay-modular-features` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-gameplay-smart-objects` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-gameplay-state-trees` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-gameplay-tags` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-media-overview` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-media-playback` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-media-subtitles` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-networking-actor-replication`, `runtime-networking-overview` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-networking-movement-prediction` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-networking-netrep` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-networking-remote-procedure-calls` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-networking-replication-graph` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-networking-transport-debugging` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-online-backends` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-online-overview` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-patching-chunk-downloading` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-patching-chunking` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-patching-install-bundles` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-physics-cloth-deformables` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-procedural-biomes` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-procedural-data-attributes` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-procedural-graph-authoring` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-procedural-node-reference` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-procedural-overview` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-procedural-vegetation` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-rhi-gpu-diagnostics` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-sequencer-cameras` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-sequencer-playback` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-tests-functional` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-tests-session-harness` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-worldgrid-data-layers` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-worldgrid-external-actors` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-worldgrid-level-instances` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-xr-ar-sessions-and-tracking` |
| <https://learn.microsoft.com/en-us/windows/mixed-reality/develop/the legacy engine/the legacy engine-hand-tracking> | 200 | `text/html` | `runtime-xr-controllers-and-hand-tracking` |
| <https://registry.khronos.org/OpenXR/> | 200 | `text/html` | `runtime-xr-openxr` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-xr-overview` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-xr-virtual-reality` |
| <<public engine documentation>> | 200 | `text/html` | `virtual-production-motion-link` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-gameplay-cameras` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-procedural-data-attributes` |
| <<public engine documentation>> | 200 | `text/html` | `runtime-tests-functional` |

## Reader/builder records

Six rows record the ADR-1441 process explicitly: four effect designs, their CPU references,
and runtime lens effects derived from the CPU reference. Each links the retained reader
behavior spec under `<workspace>`. These files were
located and their behavior-spec metadata checked; no GPL repository or source was opened,
and no GPL text was added to the tree. The archive links are local to the owner's machine;
the specs remain outside the repository.

Retained behavior-spec fingerprints (SHA-256, 2026-11-08) identify the files linked by
the rows without adding their text to this tree.

| Retained behavior spec | SHA-256 |
|---|---|
| [lens_flare behavior spec](<workspace>) | `9332e035e0cd28ff292d4f0b353924a8b62b03f39ff6092879a852a82feacc9b` |
| [radial_blur behavior spec](<workspace>) | `de69d12493680a493cbf80f47400f823b6205238a0310e038809b2743bd29585` |
| [glow_longtail_fringe behavior spec](<workspace>) | `e3a399aec07f5e80a86f0b08e377314737ed672213e723f3bb05c6930c456ebb` |
| [lut_cube behavior spec](<workspace>) | `a11a09bfdffe8fc231383b195004be45050302242b56810424a446df8ea68298` |

## Historical source references needing resolution

Two original entries cite headers outside the public-sources sets: ADR-2002 cites a
proprietary SDK's public headers alongside a permissively licensed port (ADR-2002 records
the owner's SDK-license approval; the source-class coverage remains unresolved); ADR-3373 cites
OS headers without a recorded license. Their original claims are retained with an
explicitly unresolved source-class marker, which fails the new checker. This pass did
not open those headers and cannot retroactively establish their authors' reading history.
An owner decision or source/license evidence is needed before the log can claim that every
row uses an allowed source class. See the lane report for the exact records.
