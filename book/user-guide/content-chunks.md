# Content chunks

<!-- index: Content chunk -->
<!-- index: Content manifest | see: Content chunk -->
<!-- index: DLC | see: Content chunk -->
<!-- index: base chunk | see: Content chunk -->

A **content chunk** is a named set of cooked assets that ships as one `.fpak`. The cook
writes one pak per chunk, plus `content-manifest.ron` (file, size, BLAKE3, delivery,
requirements, mount order) and `chunk-map.bin` (which chunk holds each asset). At run time
`ContentManager` (`regia_asset::content`) knows which chunks are on the device, can fetch
a missing chunk, checks it, and mounts it into the asset server. An asset whose chunk is
not mounted loads as `LoadState::Failed("unavailable:<chunk>")`. It does not crash the
process and it does not return a stand-in that looks loaded.
`AssetServer::unavailable_chunk` returns that chunk name.

Spec: `docs/specs/40-content-chunks-dlc-asset-packs.md`. The opening of that spec still
says the work is proposed. The crate, the commands below, and
[Content chunks, DLC and chunk delivery](../agents/content-chunks.md) are the behavior in
this tree. Use this page for the task. Use the agent page for every rule key, error code,
and method.

A project with no chunk rules still cooks one `base.fpak` and a one-chunk manifest. The
pak bytes match the single-pak cook.

## Rules

Rules live in `Regia.toml`. Paths are relative to `assets/`. `base` always exists and is
always installed.

```toml
[cook.chunks.base]
include = ["ui/**", "levels/main_menu.level.ron"]
pin = true

[cook.chunks.arena]
include = ["levels/arena/**"]
tags = ["arena"]
priority = 10
delivery = "on_demand"
requires = []
follow = true

[cook.chunk_options]
shared = "base"
content = "all"
```

`delivery` is `install` (in the base package), `fast_follow`, `on_demand`, or `external`.
The default is `install`. `pin = true` keeps that chunk's closure in `base`. `follow`
walks cooked dependencies. Two equal-priority rules that select the same asset fail the
cook with `chunk_rule_conflict`. A dependency claimed by several chunks at the same
priority is shared: `shared = "base"` puts it in `base`, `shared = "duplicate"` copies it
into each claiming chunk. `content = "closure"` ships only what `entry` globs and the
startup level reach. The default `content = "all"` ships every cooked asset.

A `base` asset that references another chunk is `chunk_base_reference`. A chunk that
references a chunk it does not list in `requires` is `chunk_unlisted_reference`. A
`requires` cycle is `chunk_cycle`. The full assignment, including optional references and
closure mode, is on the agent page.

## What the cook writes

`regia cook` writes, under `build/paks/<platform>/`:

| File | Role |
|---|---|
| `<chunk>.fpak` | That chunk's cooked assets. |
| `<chunk>.registry.bin` | Registry of that pak. |
| `content-manifest.ron` | Every chunk. Format `content-manifest` v1. |
| `chunk-map.bin` | Asset id to chunk. |
| `chunk-why.bin` | Why each asset landed in that chunk. |

Paks are deterministic for the same inputs. A chunk whose members, bytes, and options did
not change keeps its pak. `regia package` copies `install` chunks and the manifest into
the package `content/`, and other chunks into `dist/<platform>/chunks/` for a file host.

```console
$ regia assets chunks --platform linux
$ regia assets chunks --json
$ regia assets why assets/levels/arena/arena.level.ron
```

`regia assets why` prints the chunk and the reason (`rule`, `follows`, `shared`, `pinned`,
`default`, or `unshipped`). `regia content dlc --name <chunk> --against <dir>` cooks one
chunk against a frozen base. Drop the three files it writes into the game's `content/`.
`regia content serve <dir>` serves a folder on loopback HTTP for a manual check. It is not
a production host.

## At run time

`ContentPlugin` is idle when there is no `content-manifest.ron`. With a manifest, each
chunk is `NotInstalled`, `Queued`, `WaitingForUnmetered`, `Downloading`, `Verifying`,
`Installed`, `Mounted`, `Paused`, `NeedsUpdate`, `Failed`, or `Cancelled`. Requesting a
chunk requests the chunks it `requires` first.

On a machine that is not Android, `fast_follow` chunks are requested at startup when the
game has a content base URL. Without one, the game logs a warning and the chunks load
after a source is set and a request is made. A desktop connection is treated as unmetered.
Android asks the platform. A metered request waits in `WaitingForUnmetered` until the
player approves it.

`https://` fetches need the `content-http` feature of the `regia` crate. That feature is
off by default. Plain `http://` is accepted for this machine (`localhost`, `127.0.0.0/8`,
`::1`) without that feature. Any other `http://` host is refused with `source_not_supported`
unless `REGIA_CONTENT_ALLOW_HTTP=1` is set for that run.

`regia assets keygen` and `regia assets sign` write an Ed25519 key and a detached
`content-manifest.ron.sig`. The `regia` CLI is built with `content-signing`. A game checks
that signature when it is built with the `content-signing` feature, which `content-http`
also turns on. A game binary without the feature does not verify. The procedure is
[Signed content manifests](../agents/content-trust.md). Rollback protection, and signed
DLC entries fetched from a host, are still open (spec 40 § 11).

## Android asset packs

`regia package --platform android --format aab` ships each `fast_follow` and `on_demand`
chunk as a store asset pack. `install` chunks stay in the base module. Pack size limits
come from `[content]` (`android_pack_limit`, `android_pack_max_size`,
`android_total_max_size`). Exceeding one fails the package with `pack_limit`. The numbers
in a new project are placeholders: check them against the store's current published limits.
The cook says so on every Android package. A store upload is a separate step. This tree
does not record a completed store run.

## Shared glTF images and content chunks

<!-- index: Shared texture | see: Content chunk -->

Starting with v0.5.1, shared external textures live in the manifest at their **image
path**, such as `assets/shared.png`, or at `assets/shared.png#gltf_texture/<settings hash>`
for a settings variant. They no longer inherit chunk selectors through
`model.gltf#texture/N`. Chunk globs are relative to `assets/`. Selectors for the image
path apply to its variants as well.

For example, `assets/models/a.gltf` and `assets/models/b.gltf` both use
`assets/shared.png`:

```toml
[cook.chunks.a]
include = ["models/a.gltf"]
delivery = "on_demand"

[cook.chunks.b]
include = ["models/b.gltf"]
delivery = "on_demand"
```

Both chunks follow their material dependencies with equal default priority. No selector
matches the image itself, so this tie puts the shared texture in `base`
(`SharedPolicy::Base`, the default `[cook.chunk_options] shared = "base"`). A project
whose private glTF textures used to stay with their models changes placement on the next
cook. Base downloads and Android install-time content grow by that texture's size even
though the models are on demand.

To keep a copy with each claiming chunk, set `shared = "duplicate"`. The copies have the
same asset id. That avoids base growth and repeats the bytes in each pak. The tie rule
applies to equal highest-priority **follow claims**. Direct selectors of equal priority
that match the same image still fail with `chunk_rule_conflict`.

An explicit image selector takes precedence over follow claims. Adding `"shared.png"` to
chunk `a`'s `include` places the texture in `a`. Chunk `b` must then declare
`requires = ["a"]`. Otherwise cooking fails with `chunk_unlisted_reference`. The same
applies when the image is placed in a separate textures chunk. A base asset cannot
reference an image in an optional chunk (`chunk_base_reference`). Place that dependency
in `base` instead.

After a cook that changes this placement, recook and repack every platform, inspect
`regia assets chunks` and `regia assets why assets/shared.png`, and update image selectors
and `requires` as needed. Agents read the same placement with `content.why` and the
registry methods.

## Check

- `regia cook` exits 0. A `chunk_*` code names the rule to fix.
- `regia assets chunks` lists `base` and every chunk you declared, with a pak size.
- Loading an id from an unmounted chunk reports `unavailable:<chunk>`, and
  `unavailable_chunk` returns that name.
- Mounting the chunk makes a later load of that id succeed.
