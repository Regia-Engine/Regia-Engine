# Thumbnail request epoch isolation

This regression recipe checks the existing [Content Browser thumbnail cache](editor-asset-browser.md)
and `editor.asset_thumbnails`. A source key identifies one version of a path. A reply for key A must
retire only A's request, and must not become the current picture after the source changes to key B.

## Deterministic CPU sequence checks

Run these commands through the build host's normal wrapper:

```sh
cargo test -p regia_editor --lib epoch_isolation -- --test-threads=1
cargo test -p regia_editor --test thumbnail_epoch_isolation
```

The unit tests replace the private worker channels inside `cfg(test)`. Receiving a job and sending
its reply are explicit barriers. The production CPU worker decodes tiny generated PNGs and writes
the actual disk cache. Tests hold the replies until the requested sequence is ready. Source edits
use atomic replacement, preserve the previous modification time, and change the file length, so
key changes do not depend on timestamp resolution.

The checks cover:

- Hold A, edit and request B, then deliver A before B and B before A. The first reply leaves one
  request in flight; both replies leave zero. Only B becomes current. Repeated requests for B's
  same key do not dispatch duplicates. The two worker jobs retain their separate disk keys, and
  delivering stale A leaves B's disk bytes unchanged.
- Finish stale A after requesting B without requesting B again. The original pending B request
  must still finish.
- Let an unrelated visible tile finish and evict B from a one-picture memory cache. Re-entering B
  reads its current disk key and blue CPU pixels, with no extra generation. A separate sequence
  holds A throughout B's completion, eviction and disk re-entry, then delivers A last; B must keep
  its disk origin, pixels and disk bytes.
- Scroll B's queued tile away for a fixed number of frames while A is held. Only B expires;
  another visible asset finishes, A remains stale, and re-entry renews B normally.
- Disconnect the worker between A and B so a send discovers the failure. B and unrelated later
  work complete through the existing inline fallback.
- Disconnect with A and B outstanding so collection discovers the failure. Only the current lost
  request counts as failed. A current reply already buffered at disconnection remains successful.

Worker recovery follows the existing cache contract: a lost current request reports `failed` and
is not automatically retried. An actual source-change notification or the existing internal
`ThumbCache::forget` clears that failure; subsequent requests execute inline after worker loss.
The worker-control seam and explicit `forget` recovery sequence are internal-only primitives.
There is no agent command for controlling a worker or ordering its replies.

## Checking the oracle

The old-reply-first test was also run with the source-key rejection in `ThumbCache::accept`
temporarily disabled. It failed at `A cannot become current`, demonstrating that the CPU-state
oracle detects stale acceptance. The guard was restored and the sequence tests passed again.
Always restore a local mutation before committing. After restoring files from an archive, ensure
Cargo recompiles the changed source: archive timestamps can be older than a previously built
mutant binary, allowing that binary to be reused even though the source has been restored.

## Existing agent surface

The integration test uses `regia_agent::AgentPlugin` and its real request dispatcher, fed by an
in-process inbox. Each response is collected after one explicit app frame. It checks CPU image
pixels, `made`, `memory_hits`, `disk_hits`, failure caching, and zero `in_flight` through the normal
method:

```json
{ "method": "editor.asset_thumbnails", "params": { "paths": ["assets/blue.png"], "wait": true } }
```

The same call again adds a memory hit; adding `clear_memory: true` reads the disk image. An
undecodable source returns a thumbnail row with `state: "failed"`, rather than an RPC error.
Refused calls retain the existing lowercase `data.thumb_error` codes:

| Code | Check |
|---|---|
| `thumb_no_project` | Call before inserting an open project. |
| `thumb_bad_params` | Pass an empty `paths` array. |
| `thumb_bad_path` | Pass `assets/../outside.png`. |

These tests use no sleeps, elapsed-time assertions, random inputs, renderer windows, GPU textures,
or golden images. CPU sequence and protocol results do not establish physical platform or GPU
performance passes.
