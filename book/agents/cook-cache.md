# Shared cook cache (`cook.cache_stats`)

Every machine that cooks a project (each artist's editor, each CI runner, each build host) imports every source into
its own `.regia/cooked/<platform>/`. A **shared cook cache** (ADR-2612) is a directory that all of them point at, a
local folder or a mounted network share, or an `http://` cache server. A cook fetches the cooked outputs another machine already made from the same
inputs instead of importing again, and a read-write cook publishes what it imports. It is **off by default**.

## Turning it on

In `Regia.toml`:

```toml
[cook.shared_cache]
path = "//build-share/regia-cook-cache"   # relative paths are relative to the project
mode = "read_write"                       # or "read" (the default): fetch only, never write
```

On one machine the environment variable `REGIA_SHARED_COOK_CACHE` replaces the path (an artist's mount point is
often not the build host's). On the command line:

```
regia cook --shared-cache /mnt/share/regia-cook-cache          # read-write
regia cook --shared-cache /mnt/share/regia-cook-cache --read-only
regia cook --read-only                                          # the project's cache, read only
regia cook --no-shared-cache                                    # ignore the project's cache
```

A cache that cannot be reached is a warning: the cook goes on locally. A bad `[cook.shared_cache]` table (an unknown
key, a mode other than `read` or `read_write`) fails the cook with `shared_cache_config_invalid`'s message before
anything is cooked. `--clean` still reads the cache; add `--no-shared-cache` to import everything. A directory cache
must exist (create it once): a network share that is not mounted is then a warning instead of a cache filling the
local disk under the mount point.

A typical team: CI cooks every commit with `mode = "read_write"`, artists and other build hosts read. A machine
whose local cache is already warm fills an empty shared cache on its first read-write cook (it publishes the
sources it skipped when the cache lacks them, and remembers what it published).

## What is shared, and the key

The importers whose inputs the incremental cook already tracks (ADR-530) share their outputs: glTF / GLB, PNG, JPEG,
TGA, BMP, DDS, WAV, Ogg, FLAC and MP3, the expensive ones. A source is shared only when its `.meta` exists (commit
your `.meta` files; a first cook writes them) and, for a `.gltf`, when every buffer and image it references is a
plain relative file. Everything else (material and data files, terrain, streamed levels) cooks as before.

An entry's key is a BLAKE3 hash over: the entry format version, the **engine build id**, the platform, the source's
extension and importer version, the Android texture policy text, the project-relative source path, the source's
bytes, its `.meta`, and every other file the importer reads (a `.gltf`'s buffers and images and their `.meta`
files, `lod_classes.toml` for models, the loudness configuration for sounds), each by project-relative name and
hash. No absolute path and no machine-local stamp is in it, so two checkouts in different folders match.

The engine build id is `regia_asset_pipeline <version> <arch>-<os>` plus the `REGIA_ENGINE_BUILD_ID` the engine was
compiled with (release and CI builds stamp their commit there) or, without one, the BLAKE3 of the running executable.
One distributed engine binary therefore matches on every machine; a locally built engine matches only itself (or
identical builds), so a changed importer never reads outputs an older importer wrote.

## Safety

- One file per key, `<cache>/v1/<two hex digits>/<key>.rcc`, written to a uniquely named temp file and renamed into
  place: a reader sees a whole entry or none, and two machines cooking the same source at once both succeed and
  leave one entry.
- Every read checks the entry's magic, version, the BLAKE3 of its header, its key, platform, source, source hash and
  importer version, that it names exactly the cooked files of its outcome (an entry can never write outside the cooked cache), their lengths and their BLAKE3 hashes. A corrupt
  or partial entry is a warning (`shared cook cache …: the entry of <source> is corrupt or partial`), the source is
  imported, and a read-write cook replaces the entry.
- Read-only never creates, touches or replaces a file in the cache, and neither does `regia cook cache stats`.
- Every hash that fetches or publishes an entry is of the bytes on disk now. The incremental cook trusts a file whose
  size and modification time are unchanged without reading it; a file rewritten with both kept would fool that, so
  after a local miss the cook reads the source and its other inputs again, and a source skipped locally is read again
  before it seeds the cache. One that changed is not published (`... changed without a new size or modification
  time; not publishing it`) and imports on the next cook.
- After three failed reads or writes the cook stops using the cache for the rest of the run (one warning), so a share
  that went away does not cost a timeout per source.

## Report and maintenance

`regia cook` ends with one line: `shared cache <path> (<mode>): N hits, N misses (N corrupt), N published, N errors,
X MiB read, Y MiB written`. The same summary is in `CookReport::shared_cache`, is kept in the platform's cook
ledger (`cook_keys.bin`, where `regia cook cache stats` reads it) and is written as
`.regia/cooked/<platform>/shared_cache_summary.json` (never packed), where `cook.cache_stats` reads it.

```
regia cook cache stats [--json] [--shared-cache PATH]            # entries, bytes, last cook's summary
regia cook cache prune --max-size 500G [--shared-cache PATH]     # least recently used first
```

A read-write cook marks each entry it fetches as used, so `prune` removes entries no machine has fetched or
written for the longest time, until the cache holds at most `--max-size` (bytes, or a number with `K`, `M`, `G` or
`T`). It also removes temp files older than an hour (a writer killed mid-write).

## `cook.cache_stats`

| Param | Type | Meaning |
|---|---|---|
| `platform` | string, optional | Platform tag whose last cook to report (`linux`, `windows`, `android`); default the host. |
| `scan` | boolean, optional | List the cache for its entry count and bytes (default `true`; a large network share takes a while). |

Result:

<!-- not-a-request -->
```json
{
  "platform": "linux",
  "configured": true,
  "location": "/mnt/share/regia-cook-cache",
  "mode": "read_write",
  "last_cook": { "location": "/mnt/share/regia-cook-cache", "mode": "read_write", "engine_build_id": "…",
                 "hits": 412, "misses": 3, "corrupt": 0, "published": 3, "errors": 0,
                 "bytes_read": 9123456789, "bytes_written": 51234567 },
  "reachable": true,
  "entries": 4210,
  "bytes": 21474836480
}
```

`last_cook` is `null` until a cook of that platform used a shared cache. Without a configured cache, `location`,
`mode` and the scan fields are absent. When the scan cannot open the cache, `reachable` is `false` and
`unreachable_reason` says why (not an error). The method never writes the cache.

Error codes:

| Code | When | Fix |
|---|---|---|
| `shared_cache_config_invalid` | `Regia.toml` `[cook.shared_cache]` has an unknown key or a bad value. | Use `path = "..."` and `mode = "read"` or `"read_write"`. |

A bad `platform` or `scan` value is a plain invalid-params error.

## An HTTP cache

A path starting with `http://` uses a cache server instead of a directory: `GET`, `HEAD`, `PUT` and `DELETE` of
`<url>/v1/<aa>/<key>.rcc`, one request per connection. Any server that stores a `PUT` body whole before it serves it
works (a static file server with WebDAV-style `PUT` writes a temp file and renames it). Entries are verified the same
way. The server tracks use and does the pruning: `regia cook cache prune|stats` and `cook.cache_stats`'s scan need a
directory (the scan answers `reachable: false` for a URL). Plain HTTP has no encryption or authentication: use it on
a trusted network only.

A path starting with `https://` uses the same layout over TLS (ADR-2710): the workspace's ureq + rustls client, the
web PKI root certificates, no redirects. Set `REGIA_SHARED_COOK_CACHE_AUTH` to send an `Authorization` header with
every request (for example `Bearer <token>`); its value is never logged, printed or put in an error message, and it is
never sent over plain `http://` to another machine (that URL is refused while the variable is set; loopback is
allowed). A wrong token is an error (`HTTP 401`), counted like any unreachable cache: three errors and the cook goes on
without it. A server certificate signed by a private CA is not trusted yet (see below).

## Not built yet

An `https://` cache behind a private certificate authority: the client trusts the web PKI roots only (a CA file
setting would need PEM parsing and a direct rustls dependency; ADR-2710 defers it). The live cooker (single-file
re-cooks while the editor or `regia run --dev` runs) does not use the shared cache.
