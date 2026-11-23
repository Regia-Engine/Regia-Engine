# 00 — Vision, scope and success criteria

## 1. One-sentence goal

The **AI-agent-native** Rust game engine: an editor-first 3D engine that humans *and* AI agents can use
to build and **ship** commercial games on **Linux, Windows and Android** (Steam on desktop; macOS later),
proven by **launching Direct Order 27 on Steam with Regia Engine**.

## 2. Product goals

1. **Agent-native** (`specs/23`). Every editor capability is an agent tool (MCP + JSON-RPC): agents
   discover types, edit scenes and assets, play and test, take screenshots, build and hot-reload code
   — with undo, staged owner review, provenance and an audit log. All authored data is canonical text.
   DO27 currently uses third-party plugins for part of this workflow.
2. **Generator-friendly content pipeline.** Procedural generators are a first-class asset source with
   reproducibility checks (`regia gen verify`), matching how DO27's art is already produced.
3. **Sim/presentation split as a feature** (`specs/25`). Games with an external deterministic
   simulation (lockstep, rollback, server-authoritative) get a supported bridge, not a workaround.
4. **Fast iteration.** Build-time budgets, code hot reload/hot-patching, instant play-in-editor.
5. **Open source.** Mozilla Public License 2.0 for the engine (no fee, no royalty; game code stays the game's),
   MIT-0/CC0 templates and samples, CC-BY-4.0 docs (`07-licensing.md`, ADR-10225).

## 3. Principles (in priority order)

1. **Ships games — starting with Direct Order 27.** DO27 launches on Regia Engine, so DO27-critical work comes
   first and general-engine work after DO27 is launch-ready. Its assets are built in parallel, so content
   formats freeze early and only change with migrators.
2. **Agents and humans are equal users.** Every feature lands with its agent-protocol surface and
   agent-facing docs, or it isn't done.
3. **Desktop high-end and mobile from one architecture.** DO27 sets the desktop ceiling; Android keeps
   the design honest (tile-based GPUs, lifecycle, thermals). Quality scales via profiles.
4. **Editor-first, data-driven.** Anything a designer touches is text data, edited in the editor or by
   agents, hot reloaded. Programmers write Rust for behaviour.
5. **Rust only** for engine, editor, tools and gameplay, except media decoders and encoders, where the best library
   wins regardless of language (ADR-1593). No scripting VM. (Games may embed their own —
   DO27's simulation uses Lua for host-side mods; that is game code, not an engine feature.)
6. **Lean on the ecosystem**, own what defines the engine (`03`).
7. **Original work** (`01-cleanroom.md`, `CLEANROOM-DENY.txt`).

## 4. Platforms

| Platform | Runtime | Editor | Graphics | Minimum | Priority |
|---|---|---|---|---|---|
| Windows 10/11 x86_64 | yes | yes | DX12 (default), Vulkan | Windows 10 21H2 | DO27 (Steam) |
| Linux x86_64 (Wayland + X11), incl. Steam Deck | yes | yes | Vulkan 1.2 | Steam Linux Runtime 3.0 | DO27 (Steam), owner's dev OS |
| Android arm64-v8a (+ x86_64 emulator) | yes | no | Vulkan 1.1 | API 29, ASTC LDR | general |
| macOS (Apple Silicon + x86_64) | later | later | Metal | macOS 12 | **deferred** for release (ADR-009); built and tested on the owner's Mac, testing only (ADR-264); v0.4.0 |

Every milestone builds for the three active targets (Linux, Windows, Android); acceptance depth per
platform follows the milestone. Windows is verified in the owner's Windows VM only (`06` § 8). Not
targeted: iOS (v0.4.0 — same Metal path as macOS), web, consoles.

## 5. Scope

### Through v0.3.x / v1.0 core (milestones M0–M20, `05-milestones.md`; version table in `versions.md`)
- Platform, core, assets, renderer v1, scene/prefabs, input + runtime UI, audio (incl. parametric
  patches and voice capture), physics, gameplay framework — as specified in `specs/10`–`18`.
- **Agent protocol + MCP**, generators, staged review, provenance (`specs/23`).
- Editor with play-in-editor and code hot reload (`specs/19`, `20`).
- Animation (`specs/17`); terrain and tunnels, GPU VFX and decals, **desktop high-end renderer**
  (GPU-driven, cluster geometry, probe GI, paged shadows, volumetrics, upscaling, HDR out) (`specs/26`);
  **renderer excellence** — a reference path tracer as ground truth, numeric targets, the UE plates as the
  visual gate, zero hitches (`specs/27`).
- Packaging for all platforms incl. Android APK/AAB (`specs/21`); **Steam after the engine is stable**
  (`specs/24`, M19).
- Sim bridge and the **Direct Order 27 Regia Engine shell**, launch-ready (`specs/25`).

Items the specs mark **optional extras** (formerly "Tier 1.5") sit inside the v1.0 core: they are built only once their
milestone's acceptance already passes, and they never gate a milestone or an owner gate.

### v0.4.0 (`specs/22`)
Multiplayer framework (`regia_net`), navigation + AI, material graph editor, cinematics, 2D toolkit,
world streaming, render performance extras, UI designer, more importers (FBX), macOS and iOS.

### Parked (owner to place)
Consoles, hardware-RT-only paths, digital humans, visual-graph-to-Rust codegen, Google Play services,
accessibility (AccessKit), web, mod SDK with sandboxed logic.

## 6. Non-goals
- Compatibility with any other engine's projects, formats, APIs or naming.
- Feature-for-feature parity with a specific commercial engine (DO27's readiness gate is about *its* needs).
- A scripting VM. Running the editor on Android.

## 7. Success criteria for v1.0
The v1.0 bar is "a viable product that can be sold": see the proposed sellable checklist in
`versions.md`. The criteria below are the **engineering criteria**.

1. `regia new` → editor → level from imported/generated assets → Rust gameplay with hot reload →
   play → `regia package` for Linux, Windows and Android, without touching engine source.
2. The same loop driven **entirely by an AI agent through MCP**, with the owner only reviewing staged
   changes; agent benchmark ≥ 80% (`specs/23` § 8).
3. **DO27 is live on Steam running Regia Engine** (Gates C and D, `specs/25` § 6) on Linux, Windows and
   Steam Deck.
4. `samples/third_person` and `samples/arena` (both incl. Android; defined in `specs/14`, acceptance M20)
   ship from CI; budgets in `06` met.
5. Soak and lifecycle tests pass; user guide + agent guide + rustdoc complete.

## 8. Rough size (for planning, not commitment)
The v1.0 core is now estimated at **250k–400k lines** of Rust + WGSL (excluding dependencies) — larger than the
first estimate because the high-end renderer, terrain, VFX, agent protocol, Steam and the DO27 shell
moved into it. Gates A and B exist so the owner can stop or re-scope before most of that is spent.
Because DO27 now launches on Regia Engine, DO27's launch date follows Regia Engine's progress through M19.
