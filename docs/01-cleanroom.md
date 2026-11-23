# 01 — Cleanroom and provenance rules

Regia Engine is an **independent implementation**. It may be inspired by the *publicly observable
behaviour and workflows* of existing engines, but it must not derive from their source code.

## 1. An honest statement of what this is

A formal cleanroom separates a team that studies the original from a team that implements, with a
written spec as the only channel between them. Large language models have been trained on large
amounts of public code, and proprietary engine source has leaked into public datasets, so no AI
agent can certify that it has "never seen" a given codebase. This project therefore uses a
**documented good-faith independent implementation** process instead:

- Designs are derived from cited public sources (papers, talks, books, permissively-licensed code,
  public user documentation describing *what* a feature does).
- Proprietary source code is never consulted.
- Provenance of every non-trivial design is logged in `docs/provenance.md`.
- Output is reviewed for recognisable proprietary code before merge.

Before commercial release, the owner should have counsel review this process and the provenance log.

## 2. Allowed sources

| Source | Use |
|---|---|
| Academic papers, SIGGRAPH/GDC/HPG talks and slides, published books (e.g. *Real-Time Rendering*, *Physically Based Rendering*, *Game Engine Architecture*, *Game Programming Patterns*) | Algorithms, architecture |
| Public user-facing documentation of any engine | Understanding *what* a workflow does from a user's perspective — never its implementation, never copied text |
| Permissively licensed open-source code (MIT, Apache-2.0, BSD, zlib, ISC, CC0, BSL-1.0): Bevy, Fyrox, Godot, macroquad/miniquad, Filament docs, Recast/Detour, meshoptimizer, Jolt docs, etc. | May be read, copied, ported and adapted **with attribution** (owner decision 2026-10-08, ADR-086): original copyright and licence notice kept in the ported file's header, an entry in `docs/ported-sources.toml` (rendered into `THIRD_PARTY_NOTICES.md` by `cargo xtask notices`) and a row in `docs/provenance.md`. Never GPL/LGPL/AGPL, never MPL or other file-level copyleft, never code whose licence is unclear |
| Khronos / W3C / vendor specifications (glTF, KTX2, Vulkan, WGSL, Android NDK docs) | Freely |
| Public blog series (e.g. networking, rendering, physics articles) | Algorithms, with citation |
| The public-sources spec set: read-only behavior specs written from public documentation, including API-reference pages (owner-confirmed 2026-10-24, accepted 2026-10-28, ADR-1526) | *What* a feature does, read by a spec writer only, never by a builder; limits and the scrub list in § 3c |
| Reader/builder behavior specs for the four effects authorized by ADR-1441 (owner 2026-10-23) | The builder works from the reader's behavior spec only and never sees the GPL source; link the retained spec and record the process in each row (§ 3b) |
| Regia's own design, derivation and composition of its existing systems | Record the design and any public references separately; no outside code is implied |
| A vendor SDK whose license the owner approved in an ADR (the NVIDIA RTX SDKs license for the DLSS/NGX SDK, ADR-2002; class `licensed-vendor-sdk`, ADR-4931) | Integration against the SDK under its license only: the SDK is never committed; the provenance row names the approval ADR, the license and its public URL. A new SDK needs its own owner ADR first |
| Permissively licensed code under `Apache-2.0 WITH LLVM-exception` (LLVM, ADR-4931) | As permissive code above: the exception only adds permissions to Apache-2.0. No other license exception is accepted without an ADR |
| The owner's own game code, Direct Order 27 (owner exception 2026-11-16, ADR-8012; class `do27-owner-code`) | Engine-general DO27 Rust may be copied or adapted under the rules of § 3d and `specs/25` § 2 |

## 3. Forbidden sources and outputs

1. **Any Unreal Engine source code**, in any form: Epic's GitHub repositories, leaked copies,
   public datasets (e.g. scraped engine code on dataset hubs), forum snippets, decompiled
   binaries, shader files (`.usf`, `.ush`), header files, or reproductions from memory.
2. The same applies to any other proprietary engine (Unity, CryEngine, id Tech, Frostbite, ...)
   and to copyleft code (GPL/LGPL/AGPL) — the latter for licence, not cleanroom, reasons.
3. Parsing, reading or writing proprietary engine file formats (`.uasset`, `.umap`, `.unity`,
   `.prefab` of other engines, etc.).
4. Proprietary names, trademarks and distinctive terms in the product, APIs, docs or UI.
   Do not use: *Unreal, Blueprint, Nanite, Lumen, Niagara, MetaSound, MetaHuman, Chaos (as a
   product name), UObject, UCLASS, UPROPERTY, UFUNCTION, Pawn, GameMode, PlayerState,
   GameInstance, Sequencer, Persona, Cascade, Slate, UMG, World Partition, Fab*. Use the glossary
   in `04-conventions.md`. Naming another product in order to refer to it ("DO27's Unreal shell", the
   licence comparison in `07-licensing.md`, the migration notes in `specs/25`) is fine; using its terms
   for Regia Engine's own features is not. Common industry terms and standard editor shortcuts (player
   controller, player start, content browser, outliner, prefab, possess, play-in-editor, ...) belong to
   no one and are used wherever they are the clearest name (owner decision, ADR-009).
5. Copying documentation text, editor layouts pixel-for-pixel, icons, or default assets.

## 3a. Machine-specific deny list
`CLEANROOM-DENY.txt` (repo root) lists exact paths on the owner's machine that must never be read,
searched or opened — notably the Unreal Engine source checkout at `<workspace>`
and the old engine-project directory `DO27/` of the Direct Order 27 repository, in every commit, branch and
worktree, history included (ADR-8012 A-2). DO27's own `CLEANROOM-DENY.txt` applies as well. Rules for what in
DO27 may be read or copied are in `specs/25` § 2 and § 3d.

## 3b. Behavior-only specs from copyleft source (ADR-1441)
Narrow exception to § 3.2, recorded in ADR-1441 (owner, 2026-10-23). For features listed in `docs/provenance.md`
under ADR-1441 only, a separate reader may read a copyleft project's source and write a behavior-only spec
(no code, shader text or copied identifiers). Builders read that spec and public/permissive sources, never the
copyleft source or the reader's notes. Nothing in §§ 3 and 3a changes: Unreal, Unity and `CLEANROOM-DENY.txt`
paths stay forbidden to reader and builder alike, and any other copyleft source needs its own ADR.

## 3c. The public-sources spec set (ADR-1526, accepted by the owner 2026-10-28)
A read-only set of behavior specs written from public documentation (held outside this repo) is a source class under § 2.
Rules, in full in ADR-1526: behavior only, never literal identifiers, console variables, menu text, file paths or header
names; other engines' names never appear in Regia text; a spec writer (never a builder) reads it and writes the Regia spec in
Regia terms and the glossary; builders read only the Regia spec; provenance rows cite the public page URLs, not the set's
files. Ten hazard files are excluded or scrubbed (list in ADR-1526). §§ 3, 3a and 3b are unchanged.

## 3d. Code from Direct Order 27 (ADR-8012, owner 2026-11-16)
The owner's exception to "requirements only": engine-general Rust from the owner's own game may be copied into
Regia so that DO27 can switch to Regia's crates. Only files a DO27 handover manifest or audit names, of class P3
(or P2 after the per-file review), with the binding marks *as-is*, *adapt* (a named issue) or *requirements*
(never copied). Never: P4 (baseline-game values), P5 (privacy files), crypto, non-Rust sources (rewritten in Rust),
anything under a deny path of either repo. Each copied file has a `[[do27]]` row in `docs/ported-sources.toml`, a
`do27-owner-code` provenance row with `; approval: ADR-8012`, and the header
`// Portions copied from Direct Order 27 (<crate>/<path>, commit <sha>), ADR-8012.`; DO27 identifiers are renamed and
DO27's caps become settings. `cargo xtask notices --check` and `cargo xtask check-provenance` enforce the records.
§§ 3, 3a, 3b and 3c are unchanged.

## 4. Agent procedure

1. Before implementing an algorithm, identify a public source for it. Cite it in the PR/commit and
   add a `docs/provenance.md` row.
2. If a design detail is only known to you "from how engine X does it internally", treat that
   knowledge as unusable. Re-derive from a public source or design it fresh, and note this.
3. If you notice generated code that closely mirrors proprietary code (distinctive identifiers,
   comment phrasing, macro patterns, unusual constants), delete it and rewrite from the spec.
4. Porting permissive code (ADR-086):
   - Verify the licence of every file you take: the repository's LICENSE, the file's own header and
     the sub-crate's `Cargo.toml`. Skip anything copyleft (GPL/LGPL/AGPL, MPL) or unclear.
   - Start each ported file with the original notice followed by the licence text, e.g.
     `// Portions ported from Fyrox (https://github.com/FyroxEngine/Fyrox, commit 0876f67), Copyright (c)
     2019-present Dmitry Stepanov and Fyrox Engine contributors, MIT licence.` and then the MIT text.
     Bevy is `Copyright (c) Bevy contributors, MIT OR Apache-2.0`, used under MIT.
   - Add a `[[source]]` entry to `docs/ported-sources.toml` (project, url, licence, copyright, commit,
     files_from, files_to) and run `cargo xtask notices`. Never edit `THIRD_PARTY_NOTICES.md` by
     hand: it is generated, and `cargo xtask notices --check` (CI) fails on a hand edit, an
     incomplete entry, a non-permissive licence or a ported file without its header.
   - Add a `docs/provenance.md` row, and make the port idiomatic Regia Engine (layers, bevy_ecs, agent
     protocol, Android). Never use the upstream project's names or logos for Regia Engine features.

## 5. Recommended public references by area

These are starting points, not requirements. Verify licences before porting code.

- **Rendering:** *Real-Time Rendering* 4th ed.; Walter et al. 2007 (GGX); Burley 2012
  "Physically-Based Shading at Disney"; SIGGRAPH "Physically Based Shading" course notes (papers
  only — never engine code); Lagarde & de Rousiers 2014 "Moving
  Frostbite to PBR"; Google Filament documentation (Apache-2.0); Olsson et al. clustered shading;
  Jimenez GTAO; Hillaire atmosphere; Wronski/Persson CSM; Karis/Salvi TAA talks; Jimenez bloom
  (Call of Duty: Advanced Warfare post-processing talk); AgX tonemapping (Sobotka's public AgX work;
  as a code reference use a permissively licensed implementation such as Filament's — not Blender's
  source, which is GPL and excluded by § 3.2).
- **Physics / character control:** rapier documentation; public character-controller articles.
- **Animation:** "Animation blending" literature; two-bone IK analytic solution; Kovar & Gleicher
  motion; Ozz-animation docs (MIT) for runtime design ideas.
- **Assets:** meshoptimizer docs (MIT); KTX2/Basis specs; glTF 2.0 spec.
- **Networking (Tier 2):** Glenn Fiedler "Gaffer on Games" articles; Valve's public "Source
  Multiplayer Networking" wiki article; Overwatch GDC 2017 netcode talk.
- **Navigation (Tier 2):** Recast/Detour (zlib) and Mononen's articles.
- **Terrain (M13):** Strugar "CDLOD" paper; GPU Gems terrain chapters.
- **Virtual geometry / GI (M15b, M15d):** public meshlet/cluster-LOD papers and talks; Majercik et al.
  DDGI (JCGT 2019); Bitterli et al. ReSTIR.

## Provenance record status

Snapshot: 2026-11-08, ADR-4581; owner-requested record pass before public release.
The log contains **1,822 data rows**, each with a primary source declaration. Mixed rows
keep their other citations; counts below assign each row once.

| Primary source class | Rows | Recorded evidence |
|---|---|---|
| Public paper, book, article or talk | 1,065 | Author/title, publication or public technique description |
| Public documentation | 180 | User documentation and public API-reference pages |
| Standard | 261 | Named standards/specifications and public reference pages |
| Permissively licensed code | 82 | Source attribution and an explicit permissive SPDX license |
| Reader/builder cleanroom spec | 6 | ADR-1441 process statement and a link to the retained behavior spec |
| Regia's own design | 226 | Original design/derivation or composition of existing Regia systems |
| Unresolved historical header reference | 2 | Original claims retained pending source/license evidence; **check fails** |

All **37 marked public-sources gaps** are closed: the log rows contain **1,805 public-page
URL occurrences**, representing **1,323 distinct URLs**. Matching Regia spec provenance
sections repeat those same lists, copied only from the cited areas' Sources/reference
sections. Sixteen implementation rows that
also reference those areas received a direct page citation. Every area had a usable public
URL. The inventory and the 62 successful public-page spot checks are recorded in
[the citation inventory](evidence/cleanroom/public-sources-2026-11-08.md). Public-page
URLs, including vendor-hosted API-reference pages, remain citations under ADR-1661 (4).

`cargo xtask check-provenance`, also run by the `book-check` CLI in CI and preflight, requires four table cells, an allowed
source-class declaration and substantive evidence; permissive-code rows need their license;
set-derived rows need a public-page URL in the Source cell; reader/builder rows need the
process and retained-spec link. It rejects repository and source/header/shader-file URLs
as substitutes for set-page citations. It opens no external set, cited source or GPL
repository. The four reader-written specs remain outside the tree, with links to their
retained locations on the owner's machine; no GPL text was added here.

This documentation pass used **no external proprietary source, header, shader or
decompiled material**. It preserved two historical claims that cite non-engine headers: ADR-2002
(public SDK headers plus a permissive port, with existing SDK-license approval) and ADR-3373
(OS headers without a recorded
license). Those claims prevent an unconditional statement that every historical row
already establishes an allowed source class. Their resolution is required for a passing
record check; this section records evidence and gaps, without legal conclusions.

### Record status update 2026-11-09 (ADR-4931)

The two historical header rows are resolved and `cargo xtask check-provenance` passes with
**1,943 data rows** and no findings:

- **ADR-2002** (SDK super resolution) is declared `[class: licensed-vendor-sdk; approval: ADR-2002]`.
  The checker accepts this class only when the declaration names an approval ADR that the
  checker records as an owner license approval (today only ADR-2002, for the NVIDIA RTX SDKs
  license) and the Source cell names that license and its public URL (diagnostics
  `sdk_approval`, `sdk_license`). The class is not an alias of `permissive-code` or
  `public-documentation`.
- **ADR-3373** (Mach-O code-signature page check) no longer cites Apple's APSL-2.0 headers.
  Every constant and field offset the check reads was re-derived from LLVM's
  `llvm/include/llvm/BinaryFormat/MachO.h` and `MachO.def` (`Apache-2.0 WITH LLVM-exception`)
  and matches the code unchanged; the row is `permissive-code` with that license. The checker's
  license grammar accepts `WITH LLVM-exception` after `Apache-2.0` only.
- The 121 rows added on the integration line after the 2026-11-08 snapshot were classified in
  the same pass, with missing Notes cells filled and literal pipes escaped.

| Primary source class | Rows |
|---|---|
| Public paper, book, article or talk | 1,124 |
| Public documentation | 192 |
| Standard | 275 |
| Permissively licensed code | 83 |
| Licensed vendor SDK, owner-approved | 1 |
| Reader/builder cleanroom spec | 6 |
| Regia's own design | 262 |
