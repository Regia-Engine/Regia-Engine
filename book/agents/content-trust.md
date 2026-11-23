# Signed content manifests and chunk size budgets

A game that downloads chunks ([Content chunks](content-chunks.md)) trusts the content manifest it fetches: the manifest
holds the BLAKE3 hash of every chunk, so whoever serves the manifest decides what the game installs. `https://`
authenticates the connection to the host, not the studio that published the build. A CDN, a mirror or a compromised
static host can serve any manifest it likes. Signed manifests close that gap (ADR-2710): the studio signs each
manifest offline with a private Ed25519 key, and the game adopts a fetched manifest only when the signature verifies
under a public key it shipped with.

This page also covers the chunk size-budget gate for CI (`regia assets chunks --max-size`) and the `https://` shared
cook cache ([Shared cook cache](cook-cache.md)).

## Set up signing

1. Make a key pair, outside the project:

   ```sh
   regia assets keygen --out ~/.config/regia/keys/mygame-content.key
   ```

   It writes the private key (owner-only: mode 0600 on Unix, a DACL for this account, `SYSTEM` and `Administrators`
   on Windows; never replacing an existing file) and prints the key id and the public key, `ed25519:<64 hex digits>`. A path inside the project directory is refused with
   `content_key_in_project`: the key would be committed and shipped. Keep it in a password manager or a secrets store
   too; a lost key means shipping a build that trusts a new one.
2. List the public key in `Regia.toml`:

   ```toml
   [content]
   base_url = "https://cdn.example.com/mygame"
   trusted_keys = ["ed25519:3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c"]
   signature_policy = "required"   # or "optional" while hosts start publishing signatures
   ```

   `regia cook` and `regia package` write the keys to `content-trust.ron` next to the content manifest
   (`docs/formats/content-trust.md`). At most two keys: the current one and the next one.
3. After `regia package`, sign the manifest you upload:

   ```sh
   regia assets sign dist/linux/chunks --key ~/.config/regia/keys/mygame-content.key
   ```

   It writes `content-manifest.ron.sig` next to `content-manifest.ron` (`docs/formats/content-signature.md`), checks
   that the signature verifies, and warns when the key is not in the project's `trusted_keys`. Upload both files to
   `<base_url>/<build_id>/`. Re-sign after any change to the manifest: the signature covers its exact bytes.

**Rotating a key.** Ship a build whose `trusted_keys` lists the old and the new key. Once players have it, sign with
the new key. A later build may drop the old key.

## What the game does

At startup `ContentManager` reads `content-trust.ron` from the content directory. Without it nothing changes: fetched
manifests are accepted over `https://` (and over plain `http://` from this machine only, ADR-2138).

With it, `ContentManager::update_from_host` (agent: `content.update`) fetches `<base_url>/latest.ron`, the named
build's `content-manifest.ron` and its `content-manifest.ron.sig`, and checks the signature against the manifest's
bytes **before** it parses the manifest. Then the manifest must be for this platform and a build this game accepts
(its own or one in `compatible_with`). Only then is it adopted: chunks whose hash changed read `NeedsUpdate` (an
`install` chunk shipped in the package too, since only a fetched copy may replace it), mounted chunks whose hash did
not change stay mounted, and the `http` source moves to the new build's folder.

On any failure the running manifest, every chunk state and the sources are unchanged, and the error is one of the
codes below. A trust record that does not read (newer version, a bad key, three keys) refuses every update: the game
never falls back to unsigned content because its trust record broke.

An adopted manifest lives in memory: the next start reads the packaged one again (the patch flow for `install`
chunks is not built, see below).

## Agent methods

| Method | Class | Params and result |
|---|---|---|
| `content.update` | play | `{ base_url? }` (default: the game's content base URL). Fetch, verify, adopt. Returns `{ previous, build_id, changed, removed, signature }`. Refused while any chunk is queued, waiting for an unmetered network (`WaitingForUnmetered`), downloading, verifying or paused, or when a mounted chunk would change (`chunk_in_use`: unmount it first). Waits for the host, so call it from a launcher, a loading screen or a tool, not every frame. |
| `content.check_host` | read | `{ base_url }`: unchanged, plus `signature`; with trusted keys it fails with the signature codes below, like `content.update`, but adopts nothing. |

`signature` is `{ state, key_id, policy, trusted_keys }`: `state` is `verified` (with the signing `key_id`),
`unsigned` (policy `optional`, no signature on the host) or `not_configured` (the game trusts no key); `trusted_keys`
lists the key ids the game trusts.

## Error codes

| Code | Where | Meaning and fix |
|---|---|---|
| `content_signature_invalid` | `content.update`, `content.check_host`, `update_from_host` | The `.sig` file is malformed, names a key the game does not trust, or does not match the manifest bytes; or `content-trust.ron` did not read; or the build lacks the `content-signing` feature. Re-sign the uploaded manifest with a trusted key; check that the CDN serves the files unmodified. |
| `content_signature_missing` | same | The game requires signatures and the host serves no `content-manifest.ron.sig`. Upload it, or use `signature_policy = "optional"` for the switch-over. |
| `chunk_in_use` | `content.update` | A chunk is in flight or mounted with a changed hash. Wait, or unmount. |
| `content_key_in_project` | `regia assets keygen`, `regia assets sign` | The private key path is inside the project directory (symlinks resolved). Use a path outside every repository. |
| `content_size_budget` | `regia assets chunks --max-size` | A chunk or the total is over its budget (CLI exit 1). |
| `content_size_unmeasured` | `regia assets chunks --max-size` | `--measure disk` and a chunk has no pak yet. Cook or package first, or use `--measure memory`. |

`content.update` also answers the host codes of `content.check_host` ([Content chunks](content-chunks.md)):
`bad_params` (no `base_url` and no configured one), `manifest_missing`, `chunk_base_mismatch` (another platform or a
build the game does not accept), `source_not_supported` and `source_unreachable`.

## Chunk size budgets in CI

```sh
regia assets chunks --platform android --max-size 150MiB --max-total 1GiB --json
```

Each chunk is measured (`--measure disk`, the default: the pak's bytes, what a player downloads; `--measure memory`:
the decompressed cooked bytes) and compared with `--max-size`; the sum with `--max-total`. Sizes are bytes or take
`K`/`KiB`, `M`/`MiB`, `G`/`GiB` (binary). Over budget, the table (or the JSON) is printed and the exit code is 1 with
`content_size_budget`. The JSON:

```json
{ "measure": "disk", "max_size": 157286400, "max_total": 1073741824,
  "chunks": [ { "name": "base", "bytes": 412345678, "budget": 157286400, "over": true } ],
  "total": { "bytes": 412345678, "budget": 1073741824, "over": false },
  "over_budget": ["base"], "ok": false }
```

## `https://` shared cook cache

`[cook.shared_cache] path = "https://cache.example.com/regia"` stores entries over TLS with the same layout and the
same whole-entry semantics as `http://` and the directory backend. Set `REGIA_SHARED_COOK_CACHE_AUTH` (for example
`Bearer <token>`) to send an `Authorization` header; it is never logged and never sent over plain `http://` to another
machine. Details on [Shared cook cache](cook-cache.md).

## Verification recipe

1. `regia assets keygen --out /tmp/k/content.key` (outside the project), add the key to `[content] trusted_keys`,
   `regia package`, `regia assets sign dist/<platform>/chunks --key /tmp/k/content.key`.
2. Serve the folder (`regia content serve`) under `<build_id>/` with a `latest.ron`, run the game, and call
   `content.check_host { base_url }`: `signature.state` is `verified`.
3. Change one byte of the served manifest: `content.update` answers `content_signature_invalid` and
   `content.chunks` still shows the old `build_id`.
4. Delete the `.sig`: `content_signature_missing` under `required`.

## Not built yet

- Signed DLC entry files (`<chunk>.chunk.ron`) fetched from a host: DLC entries are read from the content directory
  only today.
- Persisting an adopted manifest across restarts, and replacing an `install` chunk in the package (ADR-1876's open
  question). A manifest that changes the mounted `base` chunk is refused with `chunk_in_use` (base cannot be
  unmounted): a new base ships as a new package.
- Freshness: an attacker who serves an older signed manifest that the game still accepts (`compatible_with`) is not
  detected; signed manifests stop forged content, not rollback to an older signed build.
- A private certificate authority for the `https://` cook cache (web PKI roots only).
