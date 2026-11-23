# Content chunks, DLC paks and chunk delivery

A project splits its cooked assets into named **chunks** with `[cook.chunks.*]` rules in
`Regia.toml`. The cook writes one pak per chunk, `content-manifest.ron` (every chunk with file, size,
BLAKE3 hash, delivery, requirements and mount order) and `chunk-map.bin` (which chunk holds each
asset). At run time `regia_asset::content::ContentManager` knows each chunk's state on the device,
fetches missing chunks from a source, verifies them, and mounts them into the asset server in
manifest order. Spec: `docs/specs/40-content-chunks-dlc-asset-packs.md`; decision records ADR-1667
(chunks, DLC) and ADR-1876 (`https://`, Android asset packs, `chunk-why.bin`, the Content Chunks
panel).

A project without chunk rules behaves as before: one `base.fpak`, plus a one-chunk manifest. The pak
is byte for byte what the single-pak cook wrote.

## Rules

```toml
[cook.chunks.base]                       # optional: pin things into base
include = ["ui/**", "levels/main_menu.level.ron"]
pin = true                               # members and everything they reach stay in base

[cook.chunks.arena]
include = ["levels/arena/**"]            # globs under assets/: `*`, `?`, `**`
entry = ["levels/arena/arena.level.ron"] # entry points: select like include, and root the shipped closure
tags = ["arena"]                         # registry tags: `key` or `key=value`
priority = 10                            # 1..=1000, default 1
delivery = "on_demand"                   # install | fast_follow | on_demand | external
requires = []                            # chunks this one may reference (base is implicit)
follow = true                            # take dependencies along (default true)
overrides = false                        # a patch: may replace ids of earlier chunks
compress = true                          # false: this chunk's pak is stored raw (default true)

[cook.chunk_options]
shared = "base"                          # base | duplicate: where a tied dependency goes
content = "all"                          # all | closure: ship every asset, or only what entries reach

[content]
android_pack_limit = 50                  # most fast-follow + on-demand packs (warning above 40)
android_pack_max_size = "512M"           # largest single fast-follow or on-demand pack
android_total_max_size = "4G"            # largest sum of install + fast-follow + on-demand chunks
```

The three Android limits are placeholders: the store changes its published limits, so check them
against its current numbers (the cook says so every Android package). Sizes are bytes or a number
with `K`, `M`, `G` or `T` (binary units).

The old shorthand `[cook] chunks = { base = ["**"] }` still parses (include only, defaults for the
rest). Unknown keys warn.

**Compression** (ADR-7971): `compress = false` writes that chunk's pak raw even when the cook
compresses (`--quality final`), so a texture chunk is read as views of the mapping with no lz4
decompress; a non-bool value is a `bad_params` rule error. Leaving the key out (or `true`) follows the
cook: `--quality fast` writes every pak raw. Changing the key rewrites only that chunk's pak. Without
the key, `regia cook --ship` already stores any blob lz4 saves less than 15 % of raw
(`PakWriteOptions::min_ratio`), which covers BC7/BC5/BC4 and ASTC textures; use the key to keep a whole
chunk raw.

**Assignment** (a pure function of the registry, the rules and the startup ids):

1. An asset selected by rules goes to the highest-priority rule. Two rules of equal priority that
   select one asset are a cook error `chunk_rule_conflict`.
2. Every chunk with `follow` walks the dependency lists breadth-first from its selected assets. A
   dependency nobody selected goes to the highest-priority claim; equal top claims from different
   chunks are **shared**: into `base`, or copied into each claiming chunk with `shared = "duplicate"`.
3. Assets selected by no rule and reached by no walk are `base` members.
4. Everything reachable from a `pin` chunk (and the startup ids) is in `base`, whatever claimed it.

Each asset gets a reason: `rule` (chunk and selector index), `follows` (the first referrer on the
shortest path, ties by smaller id), `shared`, `pinned` or `default`. Selector indices count a rule's
`include` globs, then its tags, then its `entry` globs (`entry:<glob>` in the reason table).

### Closure content: ship what the entry points reach

With `content = "closure"` the cook ships only the dependency closure of the **entry points**: every
asset an `entry` glob of any rule matches, plus the project's startup level (`[project]
startup_level`) and what it names (ADR-8092).

```toml
[cook.chunk_options]
content = "closure"

[cook.chunks.base]
entry = ["ui/main_menu.ui.ron"]

[cook.chunks.textures]
include = ["**/*.png"]                   # placement only: claims the shipped images, ships none itself
priority = 5

[cook.chunks.arena]
entry = ["levels/arena/arena.level.ron"]
include = ["levels/arena/**"]
requires = ["textures"]
```

- The assignment above runs over the shipped assets only: `include` and `tags` decide which chunk a
  shipped asset goes to, never whether it ships. An old mesh under `levels/arena/` that nothing
  references is in no chunk, no pak, no `chunk-map.bin` and no `chunk-why.bin`.
- `follow = false` changes placement only: what an entry references still ships (in `base`).
- An `entry` glob that matches no asset fails the cook with `chunk_entry_missing` (in either content
  mode: a misspelled entry would silently ship nothing). Closure content with no `entry` and no
  startup level fails with `chunk_no_entry`. Any other `content` value is a rule error (`bad_params`).
- The closure follows the cooked dependency lists, so it is as exact as the cook's references. A file
  the game opens by a path built at run time is not a reference: list it as an `entry`.
- `regia cook` prints how many cooked assets were left out. `regia assets chunks --json` gives
  `content` and `unshipped` (every left-out path; the text form lists the first 20), and
  `regia assets why <asset>` answers `chunk: null`, `reason: "unshipped"` for one of them. The editor's
  [Content Chunks panel](content-chunks-panel.md) lists them too.
- The default `content = "all"` is unchanged: every cooked asset ships, unselected ones in `base`.

**Reference checks** fail the cook: a `base` asset that references another chunk is
`chunk_base_reference` (the base game would break without it); a chunk that references a chunk it
does not list in `requires` is `chunk_unlisted_reference`; a `requires` cycle is `chunk_cycle`.

A base reference the game can live without is marked **optional** on the referring asset: the
authored tag `optional_references` (in `config/asset_tags.ron`) lists path globs, comma separated, in
the chunk-rule spelling (under `assets/`, without the prefix). A base asset's reference whose target
path matches one is a warning instead (`chunk_base_reference`, severity warning; `regia cook` prints
it and goes on, `regia assets chunks --json` gives `severity`), and at run time the target loads as
`Failed("unavailable:<chunk>")` until its chunk is mounted. The mark never relaxes a chunk-to-unlisted
-chunk reference. The cooked dependency lists do not record which field a reference came from, so
the mark is per referring asset, not per field (ADR-7236).

```ron
// config/asset_tags.ron
(
    format_version: 1,
    rules: [
        (paths: ["assets/levels/hub.level.ron"], tags: { "optional_references": "levels/arena/trophies/**" }),
    ],
)
```

## Cook, package, DLC

- `regia cook` writes `build/paks/<platform>/<chunk>.fpak`, each pak's `<chunk>.registry.bin`,
  `content-manifest.ron`, `chunk-map.bin` and `chunk-why.bin` (the reason table `content.why`
  answers from; format `chunk-why` v1, `docs/formats/chunk-why.md`). A chunk whose member ids, cooked bytes and options did
  not change keeps its pak untouched (`.chunk-inputs.ron` records the input hash and the pak hash).
  Paks are deterministic: two cooks of the same inputs give equal hashes.
- `regia package` copies `install` chunks, the manifest, the chunk map and the reason table into the
  package's `content/`, and every other chunk with a copy of the manifest into `dist/<platform>/chunks/`, ready
  for a static file host. `ship_check` expects exactly the install chunks in `content/`.
- `regia content dlc --name <chunk> --against <dir>` cooks one chunk against a frozen base (`<dir>`
  holds the base's `content-manifest.ron` and `base.registry.bin`). Dependencies the base already has
  are not copied; a selected asset whose id the base has is `dlc_id_collision` unless the rule sets
  `overrides = true` (a patch). It writes `<chunk>.fpak`, `<chunk>.chunk.ron` (its manifest entry,
  `requires: ["base@<base build id>"]`) and `<chunk>.chunk-map.bin` (its own ids, chunk-map format
  v1). Drop the three files into the game's `content/`: the asset server and `ContentManager` add
  every `<chunk>.chunk.ron` there to the manifest and merge its chunk map, so `content.chunks` lists
  the DLC, its ids answer `unavailable:<chunk>` while it is unmounted, and a DLC cooked against a base
  this build does not accept is listed but never mounted (`chunk_base_mismatch`). An entry that does
  not parse, is not named after its file, or reuses a listed chunk's name is skipped with a warning.
- Off Android no store fetches `fast_follow` chunks: when the game has a content base URL, the
  content plugin requests every missing `fast_follow` chunk at startup and mounts each when it is
  verified (`ContentManager::request_fast_follow`). Without a base URL it logs a warning; the chunks
  load after a `content.set_source` and a `content.request`.
- A chunk assignment whose asset has no cooked file (a failed or stale cook) would be left out of every
  pak and of `chunk-map.bin` (the game would read a plain NotFound, not `unavailable:<chunk>`).
  Project publication now rejects that incomplete closure with `content_dependency_missing`, detail
  `missing_cooked_file`, before writing. The existing `chunk_asset_unwritten` check remains a final
  writer safeguard (ADR-7612); its `REGIA_ALLOW_UNWRITTEN_ASSETS` override does not disable the
  required-edge check (ADR-8360). The build id needs the cache's `asset_manifest.bin`: a cache without
  one fails the pack instead of minting the same id for every such cache.
- `regia_asset_pipeline::pack_cooked_directory` (the single-call writer, no chunk rules) never packs
  the `__unmatched` placeholder chunk; `pack_cooked_directory_report` returns the cooked assets that
  are in no pak (`CookedPack::skipped`), and the plain call logs their count as a warning.
- `regia assets chunks [--platform P] [--json]` prints each chunk's delivery, asset count, disk bytes
  (the pak) and memory bytes (the cooked assets, uncompressed), and size by asset type.
- `regia assets why <asset> [--json]` prints the asset's chunk and reason with the referrer chain.
- `regia content serve <dir> [--bind 127.0.0.1] [--port 8787]` serves a folder over HTTP with `Range`
  support and no directory listing, for tests and manual checks (not for production).

## Android asset packs

`regia package --platform android --format aab` ships every `fast_follow` and `on_demand` chunk as a
store asset pack: one Gradle module per chunk under `build/android/packs/<pack>/` (module name = pack
name = the chunk name with `-` replaced by `_`; `deliveryType` `fast-follow` or `on-demand`), the pak
in the module's `src/main/assets/`, never in the base module. The app module lists them in
`assetPacks`, links the store's asset-delivery library, and the generated activity gets small helpers
(`regiaPackState`, `regiaPackFetch`, `regiaPackCancel`, `regiaPackRemove`, `regiaPackLocation`,
`regiaPackConfirm`) that `regia_platform::asset_packs` calls over JNI. `install` chunks stay in the
base module's assets; `external` chunks stay on the static host. An APK build carries the install
chunks only (it warns). A chunk named `app` collides with the app module (`pack_name_collision`).

Before any build step the package checks the store limits of `[content]` and fails with `pack_limit`
naming the chunks: more packs than `android_pack_limit`, more than one fast-follow pack, a
fast-follow or on-demand pak larger than `android_pack_max_size` (scope `pack`), or install,
fast-follow and on-demand paks together larger than `android_total_max_size` (scope `total`;
`external` chunks come from your own host and do not count).

On the device `ContentPlugin` adds the store source (`android_pack` in `content.chunks` `sources`):
a request asks the store for the pack, the store's byte counts are the transfer progress, waiting for
Wi-Fi shows the store's confirmation dialog once, and the completed pack's pak is linked into the
install directory and verified like any download. A pack the store already delivered (a fast-follow
pack after install) is hashed where it lies, once per file stamp, and used in place; one that fails
is `Failed(hash_mismatch)`. Pausing or cancelling a request cancels the store download;
`content.release` removes the pack from the device.
Store statuses map as: unknown and not installed → `NotInstalled`, pending → `Queued`, downloading and
transferring → `Downloading`, completed → `Installed`, failed → `Failed(source_unreachable)`,
cancelled → `Cancelled`, waiting for Wi-Fi or for confirmation → `WaitingForUnmetered`; anything
else → `Failed(store_unknown_status)` (logged once).

### Device check

Verified on a Fairphone 5 (Android 15) through the store's local testing mode. Package a project with
`base`, one `on_demand` chunk and one `fast_follow` chunk (`--ks` is any test keystore):

```sh
regia package --platform android --format aab --project .
bundletool validate --bundle=dist/android/<id>.aab
bundletool dump manifest --bundle=dist/android/<id>.aab --module=<pack>
bundletool build-apks --bundle=dist/android/<id>.aab --output=app.apks --local-testing --ks=<keystore> ...
bundletool install-apks --apks=app.apks
adb shell am start -n <id>/.RegiaActivity
regia install-android --project . --attach
regia agent call content.chunks --project .
regia agent call content.request '{"chunks":["<chunk>"],"mount_when_done":true}' --project .
regia agent call content.progress '{"request":1}' --project .
```

`content.chunks` lists `android_pack` in `sources`, and `content.progress` passes `Downloading` and
`Verifying` and reaches `Mounted`. `validate` lists each pack with its pak, and `dump manifest` shows
`dist:on-demand` or `dist:fast-follow`. `regia package` prints `pak_compressed` warnings for an AAB:
the bundle deflates every entry, and bundletool applies the stored setting when it builds the APKs.

The metered-network query was checked on a Pixel 9 Pro (Android 17) with an APK build of the arena
sample: `regia agent call content.network --project .` answered `platform: null`,
`platform_error: null`, `probed: true`, `metered: false` while
`adb shell dumpsys connectivity` reported `Active default network: none` (the phone had no Wi-Fi
network or SIM), and `adb shell dumpsys package <id>` listed `ACCESS_NETWORK_STATE: granted=true`.
On a phone with a network `platform` is `true` or `false`; compare it with the `NOT_METERED`
capability of the active network in the same dump.

Fast-follow packs are checked at every launch (ADR-7696): the content plugin requests every
fast-follow chunk that is not installed, so a pack the store finished is taken in place, one it is
still downloading is followed, and one that was cancelled is fetched again. Local testing never
starts fast-follow packs on its own; with this check the chunk reaches `Mounted` on the first launch
with no request. A raw file whose asset a pack chunk carries (a sound ships raw as well as cooked)
stays out of the base module.

Re-verified on the TEST Pixel 9 Pro (Android 17) on 2026-11-14 with a 475 MB on-demand chunk, so a
download lasts about 4 s: install-time chunks `Mounted` at the first frame; fast-follow `Mounted`
without a request; on-demand `Mounted`; `content.cancel` at half way reads `Cancelled` and stays
there; a new request after the cancel reaches `Mounted`; force-stop mid-download loses the request
(requests live in the process) and a new request after the relaunch reaches `Mounted`; Home
mid-download keeps the request, which reaches `Mounted` after the activity resumes in the same
process. Airplane mode changes nothing in local testing (the packs come from the device's storage):
network failures, the Wi-Fi consent dialog and the store's own fast-follow download need the store,
see `docs/release/play-internal-testing-asset-packs.md`.

Logcat without the agent (store builds): the engine logs each chunk state change at info
(`chunk state chunk="ff" from=Installed to=Mounted`, tag `regia_asset`) and the activity logs each
store status change (`ff status 4 error 0 bytes 1696090/1696090`, tag `regia_pack`; statuses as in
`regia_platform::asset_packs::status`).

## Run time

Before mounting a manifest-listed pak, the runtime compares its size and full-file BLAKE3 with
the manifest (ADR-7871), including install-time paks mounted at startup. A same-size replacement
is refused too. The check streams the file with bounded memory; it adds a sequential read at
mount time, not a per-frame check. A manager refusal is `chunk_integrity_mismatch`; at startup
`AssetServer::open` fails with `AssetLoadError::Content` carrying the same reason, so a damaged
install does not boot on a partial mount set (ADR-7979); `AssetPlugin` then exits the app at
startup with an error exit code. The length comes first (ADR-8714): a pak that is not the
manifest's length, or does not open, is a torn install, refused as described below
(`chunk_size_mismatch` or `chunk_unreadable`) while the game keeps running; only a pak of the
right length that opens is hashed. Replace a rejected installation with the matching content;
the runtime keeps the installed file for diagnosis. Unlisted paks retain their existing behavior.
The manifest supplies the expected identity; this does not add a new authenticity root.

`regia`'s default plugins add `ContentPlugin`. It reads the manifest from the asset server's content
directory at startup (no manifest: it does nothing), and adds the HTTP source when the build has a
host (below). On Android `regia` extracts the manifest, chunk map, reason table and host record from
the APK next to the extracted paks. Chunk states:

| State | Meaning |
|---|---|
| `NotInstalled` | the file is not on this device |
| `Queued` | a request exists, nothing transferred yet |
| `WaitingForUnmetered` | metered network, no request for it allows that yet |
| `Downloading` | bytes are moving (`done`, `total`) |
| `Verifying` | the file is complete; its BLAKE3 is checked |
| `Installed` | present, not mounted; manifest integrity is checked before mount |
| `Mounted` | its assets are visible |
| `Paused`, `Cancelled` | stopped; the `.part` file is kept for a later resume |
| `NeedsUpdate` | installed for another build; a request refetches it |
| `Failed(reason)` | `hash_mismatch` (failed downloads are deleted; rejected installed files are kept), `no_space`, `source_unreachable`, `store_unknown_status`, `injected`, `io`, `size_mismatch` (the file is not the manifest's length; it is not mounted) |

Requests take their chunks' requirements first and collapse repeated names; progress sums bytes over
the whole set, with present chunks counted as done. Higher priority tiers start first, then request
order, at most `max_concurrent` (default 4) at once. Transfers move a byte budget per frame
(`ContentSettings::step_budget`), so a test drives every state in a fixed order with
`ContentManager::advance(n, server)`. Fetched chunks go to the install directory (the app's private
data directory) and `content-state.ron` records what was verified.

**Metered networks.** A request without `allow_metered` waits in `WaitingForUnmetered` while the
network is metered, until `content.approve_metered` (or the network turns unmetered). `ContentPlugin`
asks the platform: on Android `ConnectivityManager.isActiveNetworkMetered()` (every Android build
declares the normal permission `ACCESS_NETWORK_STATE`); on desktop the platform cannot tell, which
reads as unmetered. The manager asks on every request and every 30 busy frames
(`METERED_POLL_TICKS`), never while idle; `content.network` shows the live answer. Tests install a
fake with `ContentManager::set_metered_probe`; an injected `metered` fault holds the network metered
whatever the platform says.

Before a transfer the manager checks the install volume for the missing bytes plus 5 percent, on top
of what running transfers still have to write; too little space is `Failed(no_space)` with no byte
written (`content.chunks` reports `free_bytes`). A store download keeps its bytes on the store's
storage, so it skips the check. A finished file hashes on the engine's I/O pool (`Verifying` until
it reports; `ContentSettings::background_verify`, `ContentManager::set_verify_runner` for tests), so
a large chunk does not stall a frame. Pausing or cancelling leaves a chunk in `Verifying` alone: the
whole file finishes and installs. A partial `.part` file is checked against the manifest's 1 MiB
block hashes (`blocks`, written for every chunk that transfers and is larger than one block) and the
transfer resumes after the last block that still matches.

A packaged chunk whose file is not the manifest's `size` (a torn or half-written install) is not
mounted: the asset server refuses it at startup and the content manager reports it
`Failed(size_mismatch)` (ADR-8810). The check is the file's length only, so an unchanged install
costs one stat per packaged pak and hashes nothing (fetched chunks are hashed once and then trusted
by the install record's size and time; `ContentManager::scan_counts` counts both). The ids the
refused pak's table of contents lists fail to load with `chunk_refused` rather than being served by
an earlier pak, and an entry of the newest pak that holds an id and does not read fails the load
with `pak_entry_corrupt`: an older pak's version of an asset is never served in its place.
`content.mount` re-checks the length and refuses a changed file with `chunk_size_mismatch`;
`AssetServer::refused_chunks` lists what the startup mount refused. Mounting a good copy (fetched
again) lifts the refusal.

A listed pak that does not open at all (a truncated file fails at its table of contents) is refused
too (ADR-8860): `refused_chunks` names it with `chunk_size_mismatch` when its length is not the
manifest's, else `chunk_unreadable`, and the ids the chunk map homes in it fail with
`chunk_refused`. An id a refused chunk lists is still served by a pak mounted after that chunk (a
newer version); only an older pak's copy is refused. A content update (`content.update`,
`ContentManager::adopt_manifest`) that changes a packaged chunk reads `NeedsUpdate`, even when the
new size differs from the packaged file's: only an unchanged entry of the wrong length is a torn
install (`Failed(size_mismatch)`).

An asset whose chunk is not mounted loads as `LoadState::Failed("unavailable:<chunk>")`, and
`AssetServer::unavailable_chunk(id)` names the chunk. Mounting a patch reloads its live assets in
place. `unmount` is refused while a load set pins assets of the chunk, unless `force`, which removes
those sets; the chunk's loaded assets then read unavailable.

## Methods

| Method | Permission | What it does |
|---|---|---|
| `content.chunks` | read | Every chunk: delivery, size, assets, state, requires, mount order; the sources. Empty (`manifest: false`) without a manifest. |
| `content.chunk` | read | One chunk: file, hash, state with bytes, first 100 asset paths, size by type. |
| `content.why` | read | `{ asset }` (path or id) → `{ asset, id, chunk, reason, rule?: { chunk, index, selector }, via, duplicated_in? }`: the cook's reason (`rule`, `follows`, `shared`, `pinned`, `default`), the selector that picked the direct asset, and the path chain back to it, from `chunk-why.bin`. A build cooked without that file answers the chunk from `chunk-map.bin` with `reason: null`. |
| `content.request` | play | Request chunks: `{ chunks, priority?, allow_metered?, mount_when_done? }` → `{ request, total_bytes }`. |
| `content.progress` | read | `{ request }` → overall state, summed bytes and each chunk's state and bytes. |
| `content.pause` / `content.resume` / `content.cancel` / `content.approve_metered` | play | `{ request }` → `{ request, state }`. |
| `content.mount` / `content.unmount` | play | `{ chunk, force? }` → `{ chunk, state }`. |
| `content.release` | play | `{ chunks, keep? }`: delete fetched files → `{ released }`. A live request that lists a released chunk ends as `Cancelled`. A retired (already finished) request whose chunk is released also reads `Cancelled` on a later `content.progress`: an agent that keeps polling after success sees the state change, so `wait_until` on `Installed`/`Mounted` before releasing, and read a late `Cancelled` after success as "the chunk was released", not a failed request. |
| `content.set_source` | play | `{ kind: "local" \| "dir" \| "http", path?, base_url? }`: where missing chunks come from. `http` fetches `<base_url>/<build_id>/<file>`: `https://` when the game is built with the `content-http` feature (`regia/content-http`; ureq + rustls, web PKI roots); plain `http://` to this machine (`localhost`, `127.0.0.0/8`, `::1`) always, to another machine only when `REGIA_CONTENT_ALLOW_HTTP=1` is set for the run (a developer override for a LAN test host: nothing authenticates plain `http://` content); otherwise `source_not_supported`. The same rule covers `content.check_host` and the packaged `content-host.ron`. |
| `content.check_host` | read | `{ base_url }`: what the host offers (`latest.ron`, then that build's manifest) against the running manifest. Returns `{ running, offered, added, changed, removed }` (chunk names) and changes nothing. Connect and each socket read time out at 30 s. On plain `http://` each response header block is also capped at 60 s, so a slow header can outlast one read. The call does that fetch twice (and a signature fetch when the game trusts a key). Use it from tools and deploy checks. `https://` needs `content-http`. An error never quotes the host's reply: a `latest.ron` that names no usable build id answers with its size, and a failed request under the id it named does not repeat the id (ADR-5869). |
| `content.inject` | play | Debug builds: `{ chunk, fault: "fail" \| "stall" \| "metered" \| "corrupt" \| "no_space", after_bytes? }`. |
| `content.network` | read | `{}` → `{ platform, platform_error, manifest, metered, probed, forced }`: `platform` is the platform's live metered answer (`true`/`false`, `null` = cannot tell, as on desktop), `platform_error` the query's error text if it failed; `metered`, `probed` and `forced` are what the content manager uses (`null`/`false` without a manifest). Works without a content manifest. |

Errors are `F0002` with `data.reason`:

| Reason | When |
|---|---|
| `manifest_missing` | the app has no content manifest or no `ContentPlugin` |
| `manifest_version` | the manifest or chunk map is newer than this build reads |
| `chunk_not_found` | no chunk of that name |
| `asset_not_found` | the asset is in no chunk (or unknown) |
| `chunk_not_installed` | mount of a chunk that is not on the device |
| `chunk_integrity_mismatch` | `content.mount` refused an installed pak of the manifest's length whose full BLAKE3 differs from its manifest entry (a wrong length is `chunk_size_mismatch`, ADR-8714); it stays unmounted and becomes `Failed(hash_mismatch)` |
| `chunk_in_use` | unmount while pinned, or release of a mounted, packaged or base chunk |
| `chunk_base_mismatch` | a DLC cooked against a base build this game does not accept (`compatible_with`) |
| `chunk_cycle` | the chunk requirements form a cycle |
| `request_not_found` | unknown request id |
| `request_finished` | pause, resume, cancel or approve of a finished request |
| `request_not_paused` | resume of a request that is not paused |
| `needs_metered_approval` | never an error: on a metered network a request's state reads `WaitingForUnmetered` until `content.approve_metered` (the name is kept for clients that match on it) |
| `no_space` | not enough free space for the missing bytes plus 5 percent |
| `hash_mismatch` | chunk failure state after a size/hash mismatch; failed downloads are deleted, rejected installed files are kept |
| `source_unreachable` | no source has the chunk, or the transfer failed |
| `source_not_supported` | the source kind is not built in (`https://` without `content-http`), or the app has no asset server |
| `bad_params` | a missing or malformed parameter, or `content.inject` in a release build |
| `io` | a file system error reading or writing a content file (the manifest, chunk map or host record, a pak being mounted) |
| `content_signature_invalid` | the game trusts signing keys and the host's `content-manifest.ron.sig` is malformed, by an untrusted key or does not match (`content.check_host`, `content.update`; [Signed content manifests](content-trust.md)) |
| `content_signature_missing` | the game requires signed manifests and the host serves no signature (same methods) |
| `chunk_size_mismatch` | `content.mount` of a chunk whose file is not the manifest's length; the chunk reads `Failed(size_mismatch)` (ADR-8810) |

Asset loads (`LoadState::Failed`, `asset.*` errors) name two more codes from the same rule:
`chunk_refused` (the asset is in a chunk whose pak the startup mount refused, for
`chunk_size_mismatch` or `chunk_unreadable`, the listed pak does not open although its length is
the manifest's) and
`pak_entry_corrupt` (the newest pak that holds the asset does not read its entry; no earlier pak
serves it instead).

The cook's own codes (`regia cook`, `regia content dlc`): `chunk_rule_conflict`,
`chunk_base_reference`, `chunk_unlisted_reference`, `chunk_cycle`, `chunk_entry_missing` (an `entry`
glob matches no asset), `chunk_no_entry` (`content = "closure"` with no entry point), `dlc_id_collision`,
`pack_name_collision`, `pack_limit`, and the root codes `content_root_missing`, `content_root_malformed`,
`content_root_unreadable`, `content_root_limit` ([Content closure roots](closure-roots.md)).

## The chunk host

`[content] base_url = "https://cdn.example.com/game"` in `Regia.toml` is checked by the cook and
written to `content-host.ron` next to the manifest (`docs/formats/content-host.md`), because a
package does not ship `Regia.toml`. At startup `ContentPlugin` adds the HTTP source for it.
`REGIA_CONTENT_BASE_URL` overrides it for one run, and `content.set_source` for the rest of a session.
`https://` needs the `content-http` feature: without it the source is refused and logged, and chunks
report `no source has <file>`. Upload `dist/<platform>/chunks/` to `<base_url>/<build_id>/` (no
upload tool is built).

## Verify it

With a packaged desktop build whose `arena` chunk is `on_demand`: copy `dist/linux/chunks/` to
`<host>/<build_id>/` (the build id is in `content-manifest.ron`) and run
`regia content serve <host> --port 8787`.

1. `content.chunks` shows `arena` as `NotInstalled`.
2. `content.set_source {kind: "http", base_url: "http://127.0.0.1:8787"}` (or `{kind: "dir", path:
   "<chunks dir>"}`) then `content.request {chunks: ["arena"], mount_when_done: true}`.
3. `play.wait_until` on `content.progress` `state == "Mounted"`: the condition is
   `{ "resource": "Content", "path": "requests.0.state", "op": "eq", "value": "Mounted" }` — the
   `Content` predicate resource is the manager's request list (`content.progress` answers, in
   request-id order: `requests.N.state/done/total/chunks`). Automated end to end by
   `crates/regia_agent/tests/content_recipe_e2e.rs` (recipe steps 3-4; the headless test registers
   a small arena level in the `LevelLibrary` by hand — its fetched pak is empty — so step 4's
   `level.open` exercises the protocol against a mounted chunk, not level bytes from the chunk).
4. Open the arena level and take a `screenshot`.
5. `content.inject {chunk: "arena", fault: "corrupt"}`, `content.release {chunks: ["arena"]}` after
   an `content.unmount`, request again: `content.progress` reads `Failed(hash_mismatch)` and the
   partial file is gone.

## Not built yet

- A running game does not switch to a newer manifest on its own: `content.update` (ADR-2710, see
  [Signed content manifests](content-trust.md)) fetches, verifies and adopts one when called, in
  memory only. Keeping an adopted manifest across restarts and patching `install` chunks in the
  package still need a decision.
- The editor panel is [Content Chunks](content-chunks-panel.md).

## Shipping updates

`assets.pak_diff { old, new, chunk_size? }` (and `regia assets pak-diff <old> <new> [--json]
[--chunk-size BYTES]`, the same report from the shell) answers what shipping the new build's paks
over the old one's costs a player. Steam delivers updates as deltas over fixed-size chunks of
about 1 MiB (SteamPipe's public documentation); Regia's pak writer keeps entries sorted by asset
id at 4096-byte alignment, so editing one asset moves only the chunks from its bytes through the
end of the pak.

Each side is a `.fpak` file or a packaged build directory (paks pair up by relative path; a pak
that exists on only one side counts as fully added or removed). The result:

- `paks`: one entry per pair — `pak`, `old_len`/`new_len` (null when the pak is added/removed),
  `assets` (`id`, `path`, `change`, `old_offset`/`new_offset`, `old_size`/`new_size`) and its
  `totals`, `chunks` and `estimate`.
- `assets[].change`: `added`, `removed`, `changed` (the stored bytes differ), `moved` (same bytes
  at a new offset — a sibling's size change shifted the layout; `old_offset` ≠ `new_offset`),
  `unchanged`.
- `chunks`: the fixed-size comparison — `unchanged`, `changed`, `added`, `removed`,
  `old_chunks`, `new_chunks`. `chunk_size` defaults to 1048576 (the ~1 MiB of Steam's public
  SteamPipe documentation); `chunk_size` sets it in bytes.
- `estimate`: `download_bytes` (the new-side bytes of every changed or added chunk) and
  `reused_bytes`. It is an estimate, never a measurement of a real Steam build: SteamPipe also
  compresses chunks, shares them across depots and reuses chunks the player already has.

The method needs the `tools` permission (desktop tools class: it reads arbitrary build paths) —
grant it in `.regia/agent.toml`. Errors: `pak_diff_unreadable` (a side does not open — missing
file, bad magic, a truncated pak is reported, never a panic), `pak_diff_no_paks` (a directory
side holds no `.fpak` files), `pak_diff_invalid_chunk_size` (`chunk_size` zero, above 64 MiB or
not an integer), `pak_diff_report` (the finished diff could not be turned into JSON; report it as a
bug). Guide topic: `pak-diff`. For players on the full Steam path see
[Shipping updates](../user-guide/steam.md) in the user guide.

## Required dependency integrity (CT-24)

Before publishing chunks or a named profile, the CLI checks every shipped asset's required
edges. `cook: Off` and `cook: Warn` affect advisory validators; they do not disable this check.
`regia assets chunks --json` reports `complete: false` and `problems` for an incomplete graph.
Assignment itself remains available for analysis of incomplete graphs.

| Code | Detail and path |
|---|---|
| `content_dependency_missing` | `detail` is `unknown_registry_id`, `excluded_required_target`, or `missing_cooked_file`. `root_path` lists registry paths from the selected root to the failing target; an unknown target is its ID. |
| `content_registry_invalid` | The registry exists but cannot be read or decoded. `root_path` names the registry file; it is never treated as an empty registry. |

Known optional targets may be deliberately excluded using the existing referring asset's
`optional_references` tag (comma-separated path globs). An unknown ID has no matching path and
remains an error, even with `*`. No field-level optional-reference syntax exists.

`agent.package` keeps these diagnostics in `agent.job_status`'s
`result.report.error.code` and `result.report.error.problems`. Follow the job until it ends,
then check the report's `ok`, not just the job transport state: a completed child process can
report a failed package. A rejected closure leaves the previous pak untouched. Existing
`chunk_asset_unwritten` and pak length/BLAKE3 checks still apply.

Reproducible CPU integration workflow on the approved build host:

```sh
cargo test -p regia_cli --test closure_integrity
```

The target exercises both shipping entry points, missing files, required and optional exclusions,
malformed registries, cycles and bounded work. Its agent workflow feeds the real shipping error
through the existing CLI tally/job report, reads it with `agent.job_status`, then verifies the
observed code and root using `play.wait_until` and `play.expect` over a test-only host event containing that report.
It uses fixed frame steps and channels, with no sleeps or wall-clock performance assertions.

## Root evidence (CT-25)

The startup ids and closure roots come from the resolver named build profiles share
([Content closure roots](closure-roots.md), ADR-8477): the startup level, the startup UI layouts,
every reached level's bakes, and each `entry` glob (an `entry` that matches only a raw level is
satisfied). Boot roots are pinned in `base`; a level a level names is a travel target, shipped in
closure mode but not pinned. `regia assets chunks --json` adds `roots` and `regia assets why` adds
`root` (`kind`, `declared_by`, `via`, `boot`).
## Shared glTF image upgrade (v0.5.1)

External glTF images now use image/variant manifest paths, so chunk selectors apply at the image
path. Equal highest-priority follow claims put an unselected shared image in `base` under
`SharedPolicy::Base`; `shared = "duplicate"` keeps copies in the claiming chunks. An explicitly
selected image keeps its selected chunk, and every other consumer chunk must list it in
`requires`. Audit base download size and Android install-time size after recooking. See the
[two-chunk example and migration steps](../user-guide/content-chunks.md).

## Texture download tiers

`regia cook --texture-tier LOW=1024 --texture-tier HIGH=2048 --texture-tier ULTRA=0`
adds complete alternative chunk directories under `build/paks/<platform>/texture_tiers/<NAME>/`.
The chunk rules above still decide membership, dependency following, shared copies, startup
pinning and delivery. The tier step reduces eligible material-map `CookedTexture` entries in their assigned chunks;
it does not introduce a `texture_tier` chunk-rule key. A `textures` chunk is named
`texture_tiers/LOW/textures.fpak`, and textures assigned to `base` remain in `LOW/base.fpak`.

Select one directory before mounting and keep its existing-format manifest, chunk map, reason
table and registry siblings together. `content.chunks` and `content.chunk` then report that
variant's sizes/hashes and unchanged logical names. For HTTP, upload under the tier manifest's
build ID; each reduced tier has a distinct ID so its download and install state cannot alias another
tier. Build DLC against that tier. The normal package command still uses the ordinary output;
hosts stage their chosen tier explicitly. Do not merge alternative directories into one content
folder: they deliberately use the same asset IDs.

`ULTRA=0` copies full paks and preserves the ordinary manifest byte for byte. Without flags the
ordinary cook is unchanged. Smaller chains are cut from cached compressed mip payloads without re-encoding. The runtime residency
cap remains effective. `--lightmap-tiers` separately derives lightmap atlases and its cache report;
it does not pick a texture package. See [texture tiers](texture-tiers.md) for validation, errors,
known scope and the existing agent methods to inspect the selected download.

Eligibility follows the runtime material residency cap (ADR-8144). UI, HDR/float, cube,
terrain VT, HLOD and voxel assets retain their full bytes, as do textures without a capped
material reference. A texture chunk may contain both reduced material maps and unchanged
textures; its name or include glob does not opt those other consumers into reduction.
