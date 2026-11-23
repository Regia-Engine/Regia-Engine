# 40 — Content chunks, DLC paks and Android asset packs

Status: proposed (ADR-1581). Plan item G9, target v0.3.x (b) (Android depth). Crates: `regia_asset` (manifest, mount,
content manager), `regia_asset_pipeline` (assignment, cook output), `regia_cli` (`cook`, `package`, `assets chunks`),
`regia_platform` (Android pack source), `regia_agent` (surface). Builders read this file plus the ambient rule docs; the facts
about paks they need are summarized in § 1 (baseline).
Reviewed: integ/specs (ADR-1585), 2026-10-28.

Source area (public-sources spec set, ADR-1526; behavior only): `runtime-patching/runtime-patching-chunking`,
`runtime-patching/runtime-patching-install-bundles`, `runtime-patching/runtime-patching-chunk-downloading` (all three exist in the
set). Everything below is re-derived in Regia terms; the set's wording, identifiers and walkthrough values are not carried over.

## 1. Purpose and scope

Finish the chunking line of `12-assets.md` § 5 ("one pak per chunk", "later paks override earlier ids", "Android asset packs map to
chunks") so that:

1. A project assigns assets to named **chunks** by rules; cooking writes one pak per chunk plus a **content manifest**.
2. A game mounts chunks in a defined order, can ask for chunks that are not installed yet, and sees a missing chunk as a clear
   "unavailable, needs chunk X" state rather than a crash or a silent hole.
3. Extra content (DLC, a patch) is built later against a frozen base and mounted over it.
4. On Android, chunks ride the store's asset-pack delivery (install-time, fast-follow, on-demand) with progress and a metered-network
   confirmation flow; elsewhere an optional HTTP source fetches chunks from a static file host.
5. Authors and agents can ask why an asset landed in a chunk and how big each chunk is.

**Baseline (implemented, spec records gaps; do not regress).**
- `regia_asset_pipeline::pak_writer::pack_cooked_directory(cache, pak_dir, chunks, compress)` takes a
  `BTreeMap<String, Vec<String>>` of chunk name to path globs, but `regia package` passes an empty map, so it writes `base.fpak`.
  It also writes `<pak stem>.registry.bin` (the asset registry of that pak: path, type, tags, dependencies; ADR-1466,
  `regia_asset::registry`).
- Pak format (frozen, `docs/specs/12-assets.md` § 4a/§ 5): `FPAK` header (magic, version, toc offset/len, content hash), 4096-aligned
  per-asset blobs compressed in 256 KiB lz4 blocks, postcard TOC with ids, types and dependencies. Desktop memory-maps; Android reads
  through AAsset streaming.
- `regia_asset::server::AssetServer` mounts every `*.fpak` in the content directory ordered by lowercased file name (then path); later
  paks override earlier ids. `LoadState` (`storage.rs`) is `NotLoaded | Loading | Loaded | Failed(String)`.
- `Regia.toml` accepts `[cook] chunks = { base = ["**"] }` (spec 21 § 2).
All of these keep working unchanged: a project with no chunk rules and no manifest behaves exactly as now. Not built: rules beyond
that shorthand, the manifest, `ContentManager`, DLC builds, store/HTTP sources, the agent surface.

Not in scope: binary delta patching (Steam's own delivery handles that, and a chunk is replaced whole), a CDN service, encryption or
DRM, payment or entitlement checks (a game decides whether to request a chunk), console store SDKs, iOS, background transfer when
the app is closed beyond what the Android store itself provides.

## 2. Model

### 2.1 Nouns

| Term | Meaning |
|---|---|
| Chunk | A named set of cooked assets that ships as one pak file. `base` always exists and is always installed. |
| Direct asset | An asset a chunk rule selects explicitly by glob or tag. |
| Dependency | An asset reached from a direct asset through the cooked dependency lists (the registry of ADR-1466). |
| Delivery | `install` (inside the base package), `fast_follow` (store fetches right after install), `on_demand` (game requests it), `external` (fetched from a source such as HTTP, or shipped as separate DLC files). |
| Content manifest | `content-manifest.ron`: lists every chunk with file, size, hash, delivery, requirements and mount order. |
| Mount | Make a pak's assets visible to the asset server. Unmount is the reverse. |
| Source | Where a chunk's file comes from at runtime: the local content directory, the Android store, or HTTP. |

### 2.2 Chunk rules (`Regia.toml`)

```toml
[cook.chunks.base]                       # implicit; may be written to pin things into base
include = ["ui/**", "levels/main_menu.level.ron"]
pin = true                               # members and their dependencies may never leave base

[cook.chunks.arena]
include = ["levels/arena/**"]            # path globs (`*`, `?`, `**`) under assets/
tags = ["arena"]                         # also direct: assets carrying these asset tags (registry tags, ADR-1466)
priority = 10                            # default 1; 1..=1000
delivery = "on_demand"                   # install | fast_follow | on_demand | external ; default install
requires = []                            # other chunks this chunk may reference (base is implicit)
follow = true                            # take dependencies along; default true
overrides = false                        # may replace ids that exist in earlier chunks (patches); default false
compress = true                          # lz4 this chunk's pak when the cook compresses; false stores it raw (ADR-7971); default true

[cook.chunk_options]
shared = "base"                          # base | duplicate : where a dependency reached from several chunks goes
```

The existing shorthand `chunks = { base = ["**"] }` (a table of name to glob array) is still read and means `include` only with every
other key at its default.

**Assignment algorithm** (deterministic; a pure function of the registry, the rules and the asset ids):
1. A direct asset takes the chunk of the highest-priority rule that selects it. Two rules of equal priority selecting one asset is a
   cook error (`chunk_rule_conflict`) naming both rules and the asset.
2. Assets selected by no rule are direct members of `base`.
3. For each chunk with `follow`, walk the dependency lists breadth-first from its direct assets. A dependency that is itself a direct
   asset keeps its own chunk. Otherwise the dependency is claimed by the walks that reach it; the claim of the highest rule priority
   wins. Claims with equal highest priority from different chunks make it **shared**: `shared = "base"` puts it in `base`,
   `duplicate` copies it into each claiming chunk (extra size, no base growth; ids stay equal, so mounting both is harmless).
4. Anything reachable from a `pin` chunk, and the project's startup level with its closure, is in `base` regardless of claims.
5. Output: a map from asset id to chunk plus a reason per asset: `Rule { chunk, rule_index }`, `Follows { via: asset id }` (the first
   referrer on the shortest path, ties by smaller id), `Shared`, `Pinned`, `Default`.

**Reference checks** (cook errors unless noted): a chunk may reference its own assets, `base`, and chunks it lists in `requires`
(transitively). A reference from `base` to any other chunk is `chunk_base_reference` (the base game would break without it), except
when the referring field is marked optional in the asset type's schema, in which case it is a warning and the runtime treats a missing
target as unavailable (§ 2.5). A requirement cycle is `chunk_cycle`.
*As built (ADR-7236):* the cooked dependency lists carry no field names, so "optional" is marked per referring asset: the
authored registry tag `optional_references` lists path globs of targets (chunk-rule spelling); such a `base` reference is a
warning with code `chunk_base_reference` (`check_references_with`). A per-field schema waits for a cooked-header bump.

**Closure content** *(as built, ADR-8092, DO27-absorb row C-03)*: `[cook.chunk_options] content = "closure"` (default `"all"`,
the algorithm above) makes the shipped set the dependency closure of the **entry points**: every asset a rule's `entry` globs
match, plus the startup ids. Step 0 computes that set; steps 1-4 then run over shipped assets only, so `include` and `tags`
place shipped assets but never ship one, and step 2's base default applies to shipped assets only. An asset outside the set
is in no chunk, no pak, no `chunk-map.bin` and no `chunk-why.bin` (`Assignment::unshipped`). `entry` globs also select their
assets as `include` does (their `rule_index` comes after the include globs and tags) in either mode. Errors: an `entry` glob
that matches no asset is `chunk_entry_missing` (in either mode: a misspelled entry would silently ship nothing); closure mode
with no entry and no startup level is `chunk_no_entry`; any other `content` value is `bad_params`. `follow = false` changes
placement only: a dependency of an entry still ships (in `base`, as an unclaimed asset does).

### 2.3 Cook output and manifest

`regia cook` and `regia package` write, next to the paks, `content-manifest.ron`:

```ron
ContentManifest(
    format_version: 1,
    build_id: "2026.10.28-1",            // from Regia.toml version + build_number + cook hash
    platform: "linux",
    compatible_with: [],                 // other build ids this build accepts as a base (DLC)
    chunks: [
        (name: "base",  file: "base.fpak",  size: 412345678, hash: "<blake3 hex>", delivery: Install,
         requires: [], overrides: false, mount_order: 0, assets: 9120),
        (name: "arena", file: "arena.fpak", size: 98765432,  hash: "<blake3 hex>", delivery: OnDemand,
         requires: [], overrides: false, mount_order: 10, assets: 1410),
    ],
)
```

- File names are `<chunk>.fpak` (`base.fpak` unchanged). Chunk names are lowercase letters, digits, `_` and `-`, start with a letter,
  at most 40 characters, case-insensitively unique (so names cannot collide on a case-insensitive file system).
- A manifest's `file` must be a bare ASCII `<stem>.fpak` (`CHUNK_FILE_EXTENSION`), unique ignoring case, or the manifest is refused:
  a fetched manifest cannot land a download on `content-state.ron`, the manifest itself or a library, and two chunks cannot share
  one file on Windows or macOS (ADR-4922).
- `size` is the file's byte length (not on-disk blocks). `hash` is BLAKE3 of the file, already a workspace dependency, used for
  verification and freshness. `mount_order` is assigned by a topological sort of `requires` (base first), ties by chunk name; it is
  stored so a runtime does not recompute it.
- A build with only `base` has no extra paks and a manifest of one chunk; the manifest is still written.
- The manifest lists `mount_order`, never absolute paths. Reading an unknown newer `format_version` fails with `manifest_version`.
- `regia package` copies each chunk according to delivery: `install` chunks into the platform package, `external` chunks into
  `dist/<platform>/chunks/` next to a copy of the manifest, so the folder can be uploaded to any static file host unchanged.
- Cook is incremental per chunk: a chunk whose member list and member cooked hashes are unchanged keeps its pak byte for byte
  (verified by hash). Pak writing of one chunk is deterministic (assets in id order), so two cooks of the same inputs give equal hashes.

### 2.4 DLC and patches

A **DLC build** cooks a new chunk against a frozen base:
`regia package --dlc <name> --against <path to base content-manifest.ron + base registry>`.

- The base registry (`<pak>.registry.bin`, ADR-1466) supplies the ids that already exist. The DLC chunk's direct assets come from its
  rules; dependencies already present in the base are not copied (they resolve through `requires: [base@build_id]`), new ones are
  included.
- An asset id present in both the DLC and the base is refused (`dlc_id_collision`) unless the chunk sets `overrides = true`, which is
  how a patch replaces a changed asset. Overrides replace by id at mount time; load order guarantees later wins.
- The manifest of a DLC chunk records `requires` with the base `build_id` it was cooked against. Mounting checks the running base's
  build id is the same or a compatible one named in `compatible_with`; a mismatch is `chunk_base_mismatch` and the chunk is not
  mounted (the game shows a "update the game" message).
- A DLC is delivered by whatever the platform uses (Steam depot, files in the content directory, the HTTP source); the engine only needs
  the pak and its manifest entry file. A pak found in the content directory with no manifest entry mounts as today (alphabetical,
  lowercased), after manifest-listed chunks.

### 2.5 Runtime

`ContentManager` (resource) wraps the asset server's mounts. Each chunk has a state:

| State | Meaning |
|---|---|
| `NotInstalled` | file not present on this device |
| `Queued` | a request exists, nothing transferred yet |
| `WaitingForUnmetered` | metered network and the player has not approved |
| `Downloading { done, total }` | bytes transferred |
| `Verifying` | file complete, hash being checked |
| `Installed` | present, not mounted; manifest integrity is checked before mount |
| `Mounted` | assets visible |
| `Paused`, `Failed(reason)`, `Cancelled` | terminal or resumable per transition table |

Transitions: `NotInstalled -> Queued -> (WaitingForUnmetered) -> Downloading -> Verifying -> Installed -> Mounted`;
`Downloading <-> Paused`; any non-terminal `-> Cancelled` (partial data kept for resume until `purge`); verification failure `-> Failed(hash_mismatch)`
and the bad file is deleted. `Mounted -> Installed` by `unmount`; `Installed -> NotInstalled` by `release`.

- **Dependencies of a request.** Requesting a chunk requests its `requires` first; the reported progress of a request is the sum of
  bytes over the whole set. Repeated names are collapsed. Chunks already installed count as done.
- **Installed integrity (ADR-7871).** Before a manifest-listed pak mounts, including startup,
  compare its regular-file size and full streaming BLAKE3 against the manifest. Mismatch is
  `chunk_integrity_mismatch`; manager state becomes `Failed(hash_mismatch)`, with the installed
  file kept for diagnosis/replacement. At startup the server open fails with that error instead of
  skipping the pak (ADR-7979). Already-mounted chunks are not rehashed per frame. Order (ADR-8714): a
  wrong length or a pak that does not open is a torn install and takes the ADR-8810/8860 refusal
  below (`chunk_size_mismatch` / `chunk_unreadable`, the game keeps running); only a pak of the right
  length that opens is hashed, and only a hash mismatch is `chunk_integrity_mismatch`.
- **Mount order.** `Mounted` chunks are ordered by manifest `mount_order`; mounting a chunk later than a mounted chunk of higher order
  re-sorts without invalidating loaded assets; an override takes effect for assets loaded after the mount (already-loaded handles keep
  their data until reloaded; the manager emits `ChunkMounted` and the asset server reloads overridden ids that are live).
- **Unmount** fails with `chunk_in_use` while load sets pinned to its assets exist; `force` first unloads those sets and invalidates
  handles into the chunk, which then read as unavailable in the same way.
- **Unavailable assets.** Loading an id that exists in the chunk map but whose chunk is not mounted yields
  `LoadState::Failed("unavailable:<chunk>")` (never a panic, never a placeholder that looks valid), and
  `AssetServer::unavailable_chunk(id) -> Option<&str>` returns the chunk. (Decision ADR-1585: no new `LoadState` variant, so existing
  exhaustive matches keep compiling.) Levels spawning such assets skip them
  and log one warning per chunk per level load. The game decides what to show.
- **Install-state queries** are cheap and synchronous from the manifest and a per-device install record (`content-state.ron`, user
  data directory, v1: chunk name to installed `build_id` and file hash). A changed `build_id` marks installed chunks `NeedsUpdate` and
  `request` refetches them; stale files are removed after the new file is verified.
- **Space.** Before a transfer the manager checks free space on the destination volume for the missing bytes plus 5 percent; failure
  is `Failed(no_space)` before any byte is written.
- **Priority tiers.** Each request has `High`, `Normal` or `Low`; the queue serves higher tiers first and, within a tier, request order.
  At most `max_concurrent` (default 4) chunk transfers run at once.
- **Metered networks.** `request` with `allow_metered: false` (default) on a metered connection goes to `WaitingForUnmetered` and
  reports `NeedsMeteredApproval`; `approve_metered(request)` lets it proceed; `approve_all_metered()` releases every waiting request.
  Where the platform cannot tell (desktop without a network-cost API) the connection is treated as unmetered.
  *As built (ADR-7236):* `regia_platform::network::active_network_metered` asks Android's `ConnectivityManager`
  (`ACCESS_NETWORK_STATE` is declared by every Android build) and answers "cannot tell" on desktop; `ContentPlugin` installs it
  as the manager's `MeteredProbe`, asked on each request and every 30 busy frames; agent `content.network` reads it.

### 2.6 Sources

```rust
pub trait ChunkSource: Send + Sync {
    fn name(&self) -> &'static str;                           // "local", "android_pack", "http"
    fn status(&self, chunk: &ChunkEntry) -> SourceStatus;     // present? where? size known?
    fn start(&self, chunk: &ChunkEntry, dest: &Path) -> Box<dyn Transfer>;
}
```

- **Local.** The content directory; a chunk file already there is `Installed` after verification (verification may be skipped for
  `install` chunks and is cached by file size + modification time in `content-state.ron`).
- **HTTP** (desktop and Android; optional feature `content-http` of `regia_asset`, off by default). Fetches `<base_url>/<build_id>/<file>`
  with `Range` requests and a `.part` file so an interrupted transfer resumes from the last verified 1 MiB block boundary (the
  manifest also carries per-chunk 1 MiB block hashes in `blocks: [..]`, optional; without them resume trusts the length and the full
  hash catches corruption at the end). `http://` uses a small dependency-free `std::net` HTTP/1.1 client written in `regia_asset` (`regia_crash` sits in a higher layer, so
  its transport cannot be reused; the two are small and independent); `https://` uses the approved `ureq` + rustls stack (docs/03,
  ADR-033, same configuration: `default-features = false, features = ["tls"]`, `ring` provider) behind the same feature.
  Decision (ADR-1585): extend the existing docs/03 `ureq` row with `regia_asset/content-http`; `cargo deny check` already runs with all features. The base URL is set by `Regia.toml`
  `[content] base_url = "https://cdn.example.com/game"` and may be overridden per run by agent or environment for tests. The manifest
  itself is fetched first from `<base_url>/<build_id>/content-manifest.ron` or `<base_url>/latest.ron` (a one-line file naming the
  newest `build_id`); a fetched manifest must have the same `platform` and a `build_id` the running game accepts (`compatible_with`).
  Origin rule (audit v0.2.12, ADR-2138): nothing signs a manifest yet, so the chunk hashes are only as trustworthy as the
  connection that brought the manifest. Plain `http://` is therefore accepted only for this machine (`localhost`, `127.0.0.0/8`,
  `::1`); a base URL on another machine must be `https://`, unless the developer override `REGIA_CONTENT_ALLOW_HTTP=1` is set for
  the run (a LAN test host); otherwise the source is refused with `source_not_supported`. Every build id (local manifest,
  `compatible_with`, `latest.ron`) must be a single safe path segment (`validate_build_id`: no `/`, `\`, space, control character
  or `..`, at most 128 bytes); the cook maps a free-text `[project] version` into one. Signed manifests are proposed in ADR-2138.
  Transfers never run in the editor's play-in-editor session unless a source is explicitly configured; the default editor flow reads
  loose assets, not paks.
- **Android store packs.** In `regia_platform` (Android only, cfg-gated, behind the existing JNI shim): `install` chunks live in the
  base bundle's asset directory and mount through the existing AAsset reader; `fast_follow` and `on_demand` chunks map to one store
  asset pack each, named by the chunk name (pack names: lowercase letters, digits, `_`, starting with a letter; the chunk-name rule above
  is chosen so every chunk name is also a valid pack name after replacing `-` with `_`; a collision after replacement is a cook error
  `pack_name_collision`). The shim exposes: pack state and byte counts, request, cancel, remove, metered-confirmation prompt, and pack
  location. A location resolves to a directory of extracted files (a pak file path) or to a path inside the app's assets; both mount.
  Location handles are released as soon as the path is read (RAII wrapper `PackLocation`, drop releases). The shim also maps each store
  status (unknown, pending, downloading, transferring, completed, failed, cancelled, waiting for unmetered network, not installed, remove
  pending/failed) onto `ChunkState`; unlisted statuses map to `Failed(store_unknown_status)` and are logged once.
  Store limits that matter at cook time (verify against the store's current published limits when building; they change): a bounded
  number of packs per app (the cook warns above 40 and errors above the configured limit, default 50), at most one install-time pack
  and one fast-follow pack, per-pack and total size caps. `regia package --platform android` fails with `pack_limit` before building if
  a limit is exceeded and prints the offending chunks. *As built (ADR-7236):* `[content] android_pack_max_size` (one fast-follow or
  on-demand pack, default 512 MiB) and `android_total_max_size` (install + fast-follow + on-demand, default 4 GiB; `external`
  chunks do not count), both placeholders to verify against the store's published limits.
  `regia package --platform android` generates the asset-pack Gradle modules from the manifest (one module per non-install chunk, with
  the delivery type in its build file, the module name equal to the pack name) in the generated shim (allowed non-Rust source, spec 21
  § 4) and builds an AAB; the pak files are placed in the module's asset directory. Pak files must not also stay in the base module.
  Tests that need the store or a device are listed in § 8.

## 3. API shape in Rust

```rust
// regia_asset::content
pub struct ContentManifest { pub format_version: u32, pub build_id: String, pub platform: String,
                             pub compatible_with: Vec<String>, pub chunks: Vec<ChunkEntry> }
pub struct ChunkEntry { pub name: String, pub file: String, pub size: u64, pub hash: String, pub delivery: Delivery,
                        pub requires: Vec<String>, pub overrides: bool, pub mount_order: u32, pub assets: u32,
                        pub blocks: Vec<String> }
pub enum Delivery { Install, FastFollow, OnDemand, External }
pub enum ChunkState { NotInstalled, Queued, WaitingForUnmetered, Downloading { done: u64, total: u64 }, Verifying,
                      Installed, Mounted, Paused, NeedsUpdate, Failed(FailReason), Cancelled }
#[derive(Resource)] pub struct ContentManager { /* manifest, states, sources, queue */ }
impl ContentManager {
    pub fn manifest(&self) -> &ContentManifest;
    pub fn state(&self, chunk: &str) -> Option<ChunkState>;
    pub fn request(&mut self, chunks: &[&str], opts: RequestOptions) -> Result<RequestId, ContentError>;
    pub fn progress(&self, id: RequestId) -> Option<RequestProgress>;      // done, total, per-chunk, state
    pub fn pause(&mut self, id: RequestId); pub fn resume(&mut self, id: RequestId); pub fn cancel(&mut self, id: RequestId);
    pub fn approve_metered(&mut self, id: RequestId); pub fn approve_all_metered(&mut self);
    pub fn mount(&mut self, server: &mut AssetServer, chunk: &str) -> Result<(), ContentError>;
    pub fn unmount(&mut self, server: &mut AssetServer, chunk: &str, force: bool) -> Result<(), ContentError>;
    pub fn release(&mut self, chunks: &[&str], keep: &[&str]) -> Result<(), ContentError>;  // delete files except `keep`
    pub fn chunk_of(&self, id: AssetId) -> Option<&str>;                    // from the registry
}
pub struct RequestOptions { pub priority: Tier, pub allow_metered: bool, pub mount_when_done: bool }
pub enum Tier { Low, Normal, High }
#[derive(Message)] pub struct ChunkStateChanged { pub chunk: String, pub from: ChunkState, pub to: ChunkState }
pub struct ContentPlugin;                  // reads the manifest at startup (absent manifest = no-op, today's behaviour)

// regia_asset_pipeline::chunks
pub struct ChunkRules { /* parsed from Regia.toml */ }
pub struct Assignment { pub chunk_of: BTreeMap<AssetId, String>, pub reason: BTreeMap<AssetId, Reason> }
pub fn assign(registry: &AssetRegistry, rules: &ChunkRules, startup: &[AssetId]) -> Result<Assignment, ChunkError>;
pub fn check_references(registry: &AssetRegistry, a: &Assignment, rules: &ChunkRules) -> Vec<ChunkProblem>;
pub fn write_chunks(cache: &Path, out: &Path, a: &Assignment, rules: &ChunkRules, compress: bool) -> Result<ContentManifest, ChunkError>;
```

`regia_asset` stays in the World layer (no upward dependency); HTTP lives behind its optional feature; the Android source lives in `regia_platform` and registers
itself with `ContentManager::add_source`. `regia_asset_pipeline` is desktop-only and never a runtime dependency. Errors use
`thiserror`, carry chunk or asset, map to the agent codes in § 5. Tracing spans: `content_assign`, `content_write_chunk`, `content_mount`,
`content_transfer`, `content_verify`.

## 4. Files and formats

| Item | Format | Notes |
|---|---|---|
| `content-manifest.ron` | v1, new format name `content-manifest` in `check-formats` | Additive fields keep v1; incompatible changes bump and ship a migrator. Older runtimes ignore a manifest they cannot read only when `format_version` is lower than theirs (a newer manifest is `manifest_version`). |
| `content-state.ron` | v1, user data directory, format `content-state` in `check-formats` (ADR-7236, `docs/formats/content-state.md`) | A cache; deleting it is always safe (everything re-verifies). |
| `Regia.toml` `[cook.chunks.*]`, `[cook.chunk_options]`, `[content]` (`base_url`, `android_pack_limit`) | additive keys | Old shorthand still valid. Unknown keys warn. |
| Pak files | unchanged (`FPAK` header and TOC of spec 12 § 5) | No pak version bump: chunk identity lives in the manifest, so the format freeze (spec 12 § 4a) is untouched. |
| `<pak>.registry.bin` | unchanged | Used for DLC id checks; a chunk name per record is not added (the manifest maps assets to chunks through the cook's assignment, written to `chunk-map.bin` next to the manifest: sorted `(asset id, chunk index)` pairs, `format_version` 1, so the runtime can answer `chunk_of` without the registry). |

Docs: `docs/formats/content-manifest.md` with an example manifest and `Regia.toml` excerpt; the lead runs `cargo xtask check-formats --update`
and `cargo xtask manual`.

## 5. Agent-protocol surface

| Method | Class | Params | Result |
|---|---|---|---|
| `content.chunks` | read | `{}` | `{ build_id, chunks: [{ name, delivery, size, assets, state, requires, mount_order }] }` |
| `content.chunk` | read | `{ name }` | one chunk plus first 100 asset paths and size by asset type |
| `content.why` | read | `{ asset }` (path or id) | `{ chunk, reason: "rule"/"follows"/"shared"/"pinned"/"default", rule?, via: [asset path chain to a direct asset] }` |
| `content.request` | play | `{ chunks, priority?, allow_metered?, mount_when_done? }` | `{ request, total_bytes }` |
| `content.progress` | read | `{ request }` | `{ state, done, total, chunks: [{ name, state, done, total }] }` |
| `content.pause` / `content.resume` / `content.cancel` / `content.approve_metered` | play | `{ request }` | `{ request, state }` |
| `content.mount` / `content.unmount` | play | `{ chunk, force? }` | `{ chunk, state }` |
| `content.release` | play | `{ chunks, keep? }` | `{ released }` |
| `content.set_source` | play | `{ kind: "local"/"http", base_url? }` | `{ source }` (tests and tools) |
| `content.inject` | play | `{ chunk, fault: "fail"/"stall"/"metered"/"corrupt"/"no_space", after_bytes? }` | `{ armed }` (debug builds, for deterministic tests) |
| `content.network` | read | `{}` | `{ platform, platform_error, manifest, metered, probed, forced }` (ADR-7236) |

Errors are `F0002` with `data.reason` set to one of (lowercase, documented on the book page): `manifest_missing`, `manifest_version`, `chunk_not_found`, `asset_not_found`, `chunk_not_installed`, `chunk_in_use`,
`chunk_base_mismatch`, `request_not_found`, `request_finished`, `needs_metered_approval`, `no_space`, `hash_mismatch`,
`source_unreachable`, `source_not_supported`, `pack_limit`, `pack_name_collision`, `chunk_rule_conflict`, `chunk_base_reference`,
`chunk_cycle`, `dlc_id_collision`, `bad_params`, `not_playing`. The cook adds `chunk_entry_missing` and `chunk_no_entry`
(closure content, ADR-8092).

CLI: `regia assets chunks [--platform P]` prints the table of `content.chunks` plus per-type sizes from the cooked cache (disk size =
pak bytes; memory size = decompressed cooked bytes; both columns always shown, since they differ), `regia assets why <asset>` prints the
chain, `regia content serve <dir>` starts a local static file server over a `dist/<platform>/chunks` folder (HTTP, used by tests and
manual checks; no directory listing, correct `Range` support; not for production). Book page `book/agents/content-chunks.md`, COVERAGE row
"Content chunks / DLC / asset packs". Verification recipe for the guide: (1) `content.chunks` shows `arena` as `NotInstalled`;
(2) `content.set_source` http to the local server and `content.request`; (3) `play.wait_until` on `content.progress.state == "Mounted"`;
(4) open the arena level and `screenshot`; (5) `content.inject` `corrupt` and see `Failed(hash_mismatch)` with the partial file removed.

## 6. Editor surface (brief)

A "Content" panel lists chunks with size bars (disk and memory toggle), the rule that selected each direct asset, and a "why is this
asset here" action on any asset in the browser. Reference-graph and size-map views of the asset tools (plan item G1) open on a
chunk. Chunk rules are edited in the project settings panel as the TOML keys above.

## 7. Determinism notes

Assignment, the manifest, `mount_order` and pak contents are pure functions of registry + rules + inputs (ordered maps, ids sorted,
reason chains tie-broken by smaller id). The same inputs give byte-identical paks and equal hashes on every host platform. Runtime
transfers are not deterministic in time; their state machine is: with an injected source and a fixed step sequence (`Source::advance(n_bytes)`)
every state and event order is reproducible, which is how the tests run.

## 8. Tests and acceptance

First, as failing tests. No wall-clock asserts; the network tests use a loopback test server started in the test on an OS-assigned port
and a manual step API (`advance(n)`) instead of sleeps. Temp dirs from `unique_temp_dir`.

Assignment and cook:
1. Rules: direct by glob and by tag; highest priority wins; equal priority conflict is `chunk_rule_conflict`; unselected assets go to base.
2. Dependencies follow the highest-priority claim; equal claims produce `Shared`, placed in base or duplicated per `shared`; `follow = false` leaves
   dependencies to default/base.
3. `pin` and startup-level closure stay in base even when a rule claims them.
4. Reasons: for a depth-3 chain the `via` list is the shortest path, ties by smaller id.
5. Reference checks: base to chunk is `chunk_base_reference` (warning when optional); chunk to unlisted chunk errors; cycle `chunk_cycle`.
6. Writing: N chunks give N paks and a manifest whose sizes and BLAKE3 hashes match the files; two cooks give equal hashes; an unchanged
   chunk keeps its pak when another chunk changes; a project with no rules still writes `base.fpak` and a one-chunk manifest, and its pak
   is byte-identical to today's output for the same input.
7. Old shorthand `chunks = { base = ["**"] }` parses with defaults.
8. DLC: against a frozen base, colliding id is `dlc_id_collision`; `overrides = true` passes; dependencies already in base are not copied;
   `requires` records the base `build_id`.

Runtime:
9. Mount order follows `mount_order`, not file names; a manifest-less pak mounts after listed chunks alphabetically (backward compatible).
10. Override: a patch chunk replaces an id; assets loaded afterwards read the patch; a live handle gets a reload event.
11. State machine: every legal transition and every illegal call (`resume` of a non-paused request, `mount` of a non-installed chunk
    gives `chunk_not_installed`).
12. Requests collapse repeated names, include `requires` first, and report summed progress; a cancelled request keeps `.part` data and a
    new request resumes from it (loopback server counts bytes served: the second transfer fetches only the missing tail).
13. Verification: a flipped byte gives `Failed(hash_mismatch)`, deletes the file, leaves other chunks untouched.
14. No space (injected) fails before writing; metered (injected) waits until `approve_metered`; `approve_all_metered` releases all.
15. Priority tiers: with `max_concurrent = 1`, a `High` request submitted after a `Low` one starts first once a slot frees.
16. `Unavailable { chunk }` for an asset in an unmounted chunk; a level load that references it logs once per chunk and spawns the rest.
17. `unmount` is `chunk_in_use` while a pinned load set exists; `force` unloads and invalidates.
18. `build_id` change marks installed chunks `NeedsUpdate`; `chunk_base_mismatch` for a DLC built against another base.
19. HTTP source end to end on loopback (`http://`): manifest fetch, range resume, hash verify, mount. `https://` (feature on) needs a
    certificate fixture: the test is a TODO for the lead if no fixture exists.
20. Agent: round trip of every method, the five-step recipe in § 5, `cargo xtask agent-coverage` and `check-formats` pass.

Needs a device, a store or a GPU (the lead runs these; the builder only writes them and marks them ignored-by-default):
21. Android: `regia package --platform android` for a three-chunk project produces an AAB with the expected asset-pack modules (inspect with the
    bundle tool's `dump manifest` and `build-apks` for a local device spec); install-time chunk mounts from the base bundle on a device; on-demand
    chunk request through the local bundle-testing mode shows `Downloading` then `Mounted` on a real device (`adb` log + `content.progress` over the
    agent bridge). Exact commands are recorded in the book page when verified.
22. Pause/resume of the activity during a download (Android surface lifecycle, AGENTS.md § 5) leaves the request intact.

Acceptance of the feature: a project with `base` + one `on_demand` chunk cooks to two paks and a manifest; a desktop run with the HTTP
source fetches, verifies and mounts the chunk and the game loads its level; an Android build ships the on-demand chunk as a store pack.

## 9. Platform notes

- Linux, Windows: local and HTTP sources; paks are memory-mapped (spec 12 § 5b), and a mounted pak's handle denies other writers on
  Windows, so `unmount` must close the mapping before `release` deletes the file; `release` of a mounted chunk is `chunk_in_use`.
- Android aarch64: store source via JNI; `install` chunks are read through AAsset streaming (no memory-map); fast-follow and on-demand packs
  are real files and memory-map. Surfaces and activity can pause mid-download: transfers belong to the store or to a plain thread
  that survives surface loss, not to the surface. Writes go to the app's internal files directory, never the shared storage.
- macOS: testing-only; local and HTTP sources work, no store source.
- New dependencies: none required. HTTP uses `std::net` (http) and the approved `ureq` + rustls (https) behind `content-http`; BLAKE3
  is already a dependency. Any change needs the docs/03 entry and `cargo deny check`.

## 10. Performance budget (proposed, to be measured)

| Operation | Budget | Span |
|---|---|---|
| Assignment over 100,000 assets and 500,000 dependency edges | at most 400 ms (desktop tool) | `content_assign` |
| Manifest load and state setup | at most 5 ms | `content_mount` |
| Mount of a pak (index only; pages fault in) | at most 10 ms for 50,000 entries | `content_mount` |
| Hash verification | at least 1 GB/s on desktop, runs on the I/O pool | `content_verify` |
| Per-frame cost of the manager with idle requests | at most 0.02 ms | `content_tick` |
| Transfer loop | does not allocate per chunk of 64 KiB read after warm-up | `content_transfer` |

## 11. Out of scope and follow-ups

Delta patches inside a pak; a CDN or build-upload service; encryption of manifests; background transfer when the app is
closed on desktop; iOS on-demand resources; a game-facing "download manager" UI (the templates may add one later).

Built since (ADR-2710): **manifest signing** (a detached Ed25519 `content-manifest.ron.sig` next to the hosted manifest,
checked before the manifest is read against the keys of `content-trust.ron`, cooked from `[content] trusted_keys`;
`regia assets keygen` / `regia assets sign`; `content.update` adopts a verified manifest; codes
`content_signature_invalid`, `content_signature_missing`, `content_key_in_project`) and **size-budget gating in CI**
(`regia assets chunks --max-size SIZE [--max-total SIZE] [--measure disk|memory] [--json]`, exit 1 with
`content_size_budget`). Still open: rollback protection, signed DLC entries fetched from a host, persisting an adopted
manifest. See `book/agents/content-trust.md`.

### As built: refinements recorded by the conformance audit (ADR-5636)

The code legitimately refines the text above in these places. ADR-1667, ADR-1876, ADR-2138, ADR-2710 and ADR-4922 already
record most of them; this list puts them next to the text.

- **DLC command** (ADR-1667 f): `regia content dlc --name <name> --against <base>`, not `regia package --dlc` (§ 2.4).
- **Mount order** (ADR-1667 h): `mount_order` is the position in the topological order (0, 1, 2, ...), so the § 2.3 example's
  `mount_order: 10` reads `1` in a real manifest.
- **Android packs** (ADR-1876): the store binding is `regia_platform::asset_packs` plus `regia_asset::content::StorePackSource`;
  there is no `PackLocation` wrapper (§ 2.6). Per-pack and total size caps are not checked yet (listed as a gap).
- **Mount event**: a mount is `ChunkStateChanged { to: Mounted }`; there is no separate `ChunkMounted` message (§ 2.5).
- **Update detection**: an installed chunk is `NeedsUpdate` when its recorded **hash** differs from the manifest's, not when
  only the `build_id` changed, so a new build that leaves a chunk byte-identical does not refetch it (§ 2.5).
- **Metered networks**: a waiting request reads state `WaitingForUnmetered` in `content.progress`; `needs_metered_approval`
  is never returned as an error (§ 2.5, § 5). No platform network-cost query feeds `ContentManager::set_metered` yet, so
  outside the Android store and `content.inject metered` every connection reads as unmetered (§ 2.5's desktop rule).
- **API shape** (§ 3): `pause`, `resume`, `cancel` and `approve_metered` return `Result<String, ContentError>` (the new
  state label); `unmount(server, sets: Option<&LoadSets>, chunk, force)` takes the load sets it checks; `release` returns
  the released names; `write_chunks(cache, out, a, rules, opts: &ChunkWriteOptions)` returns the manifest and a
  `ChunkWriteReport`.
- **Reason codes** (§ 5): also `request_not_paused` and `chunk_unlisted_reference` (ADR-1667 d), `io` (file system),
  the failure codes `store_unknown_status` and `injected` (inside `Failed(..)`), and the signing codes of ADR-2710.
  `not_playing` is not produced: the content methods work in any world that has a `ContentManager`. The cook's reference
  errors lead with their code (`chunk_base_reference: ...`).
- **Optional-field references** (ADR-1667 c): a base reference through an optional field is still an error; the registry
  carries no field schema yet.
- **Coverage rows**: the methods are listed in `book/COVERAGE.md` per method and under the content rows, not under one row
  named "Content chunks / DLC / asset packs".
- **Packaged chunk length** (ADR-8810): a packaged pak whose length is not its entry's `size` is not mounted at startup
  (`AssetServer::refused_chunks`, code `chunk_size_mismatch`) and reads `Failed(size_mismatch)` in the content manager;
  the ids its table of contents lists fail to load (`chunk_refused`) instead of being served by an earlier pak. A packaged
  pak is still never hashed at mount: the length check reads the open reader's length (no extra file access), and the
  whole-file check stays the background `pak_verify` (once per changed pak). `content.mount` re-checks the length.
- **No fall back past a bad entry** (ADR-8810): the newest mounted pak that holds an id decides; an entry there that does
  not read fails the load with `pak_entry_corrupt` (§ 2.5's "later chunks win" applies to failures too).
- **Unopenable listed paks, updates, newer paks** (ADR-8860): a listed pak that does not open (a truncated file) is refused
  too (`chunk_size_mismatch` by its length, else `chunk_unreadable`) and the ids the chunk map homes in it fail with
  `chunk_refused`; a pak mounted after a refused chunk still serves the ids it holds. A manifest update that changes a
  packaged chunk reads `NeedsUpdate`, not `Failed(size_mismatch)`, whatever the new size.
- **Fast-follow at launch** (ADR-7696): on Android the content plugin requests every fast-follow chunk that is not
  installed at every launch (the store's guide asks the app to check them and resume a cancelled one; local testing never
  starts them). Chunk state changes are logged at info and the activity logs store status changes (logcat tag
  `regia_pack`), so a store-track build without the agent can be checked from logcat.

## 12. Provenance and open questions

| Area | Algorithm/behavior | Public source |
|---|---|---|
| Assets / chunk assignment | Rules with priority, dependency following with highest-priority claim, shared dependency handling, base chunk invariant | Rule-based partitioning of a dependency graph (breadth-first reachability) is textbook; behavior-only read of the public-sources set area runtime-patching (ADR-1526). Rule syntax, tie rules and `Reason` are Regia's own. |
| Assets / content manifest and mount order | Manifest of chunk files with size and version, hosted statically, fetched before content; dependent chunks first | Static-host manifest-then-files download pattern; format (RON, BLAKE3, mount order) is Regia's own; HTTP `Range` per RFC 9110 § 14. |
| Platform / Android asset packs | Install-time, fast-follow and on-demand delivery; pack states, metered confirmation, location handles | Google Play "Asset Delivery" public developer documentation (developer.android.com/guide/playcore/asset-delivery) and the Play Core asset-pack API reference. Mapping of states is Regia's own. |
| Assets / content hashing | BLAKE3 file hash | BLAKE3 specification (https://github.com/BLAKE3-team/BLAKE3-specs). |

Decisions (ADR-1585), replacing the writer's open questions: (a) provenance cites public standards and the store's public
documentation (below), not the set's files. (b) Store pack limits are checked against `[content] android_pack_limit` (default 50, a
placeholder) and the build prints a warning to verify against the store's current limits. (c) `shared = "base"` stays the default for
tied dependencies. (d) `https` ships in the same cut behind `content-http`; its loopback TLS test needs a certificate fixture and is a
lead TODO if none exists. (e) `chunk-map.bin` stays: the runtime must answer `chunk_of` without loading a registry.

### Owner decisions

None.

### Public-page citations

Sources-section URLs recorded by the spec writer on 2026-11-08 (ADR-1526; ADR-1661).
These are documentation/reference citations, including public API-reference pages; only the
URLs were transcribed from the external set.

- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>

### Required-edge integrity at publication (ADR-8360, CT-24)

Chunk assignment remains permissive for graph analysis. Its reference report separately identifies
incomplete required edges; shipping runs the check before writing a chunk or named-profile pak,
including when advisory cook validation is `Off` or `Warn`. `content_dependency_missing` includes
`detail` (`unknown_registry_id`, `excluded_required_target`, `missing_cooked_file`) and `root_path`
from the shipped root to the failing edge. Cycles of asset references are legal and visited once.
A malformed registry fails with `content_registry_invalid`; only an absent registry can use the
legacy manifest fallback. The existing per-referrer `optional_references` tag may intentionally
exclude a known target matched by its path glob. Unknown IDs do not have paths and cannot be
made optional by that tag. Field-level optional-reference syntax remains deferred. Existing
unwritten-asset and pak hash/length checks remain in place. No serialized format changes.
