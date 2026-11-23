# Pipeline caches in feedback and packages

`feedback.capture` attaches the renderer's last saved `.fpc` to a normal feedback
bundle as `pipelines/pipelines-<backend>-<vendor>-<device>-<key-hash>.fpc`.
`feedback.list { "name": "<bundle name>" }` returns that entry in `files` and its
name in `feedback.pipeline_cache`. `feedback.pipeline_cache_missing` explains an
omission when a configured saved file could not be attached. Existing RPC errors
and permissions are unchanged; an omitted cache does not fail feedback capture.

```json
{"jsonrpc":"2.0","id":1,"method":"feedback.capture","params":{"category":"performance","screenshot":false}}
```

Wait for the capture reply, then call `feedback.list` with the returned `name`.
Check that `feedback.pipeline_cache` names an entry and
`feedback.pipeline_cache_missing` is null. No renderer or saved cache means no
attachment. Feedback never forces a driver snapshot; pipelines compiled since
the last normal cache checkpoint are absent. Automatic crash bundles omit caches.

Hosts can set `FeedbackConfig::pipeline_cache_max_bytes` (32 MiB by default,
0 disables; capped to half the 256 MiB bundle reader limit). Hosts with an external
renderer can insert `feedback::PipelineCacheAttachment { path, key }`; the key
must be the full expected `PipelineCacheKey`. The worker reads only that file,
checks its size, exact key and checksum, and omits stale, invalid, missing or
oversized data. The engine writes locally and uploads nothing.

## Package trusted tester caches

```sh
regia package --platform linux --pipeline-caches trusted-feedback/
regia package --platform windows --pipeline-caches feedback.zip \
  --pipeline-cache-reference current-build-warm-cache.fpc
```

The input is one `.fpc`, a stored feedback ZIP, or a directory containing either
recursively. `--pipeline-cache-reference PATH` is repeatable and accepts the same
input forms. A reference must contain an intact cache produced by the current game
build, including any content salt; its GPU and driver may differ from the tester's.
A harvested cache is eligible only when its shader hash and wgpu version match
one reference pair. Without references, feedback caches must match this engine's
unsalted shader hash and wgpu version. Loose cache files keep their existing stale
warning behavior unless references explicitly constrain them too.

Each complete GPU/backend/driver/wgpu/shader key gets its own shipped filename.
The largest intact candidate wins; equal sizes choose the lowest BLAKE3 digest of
the complete file. This is a selection heuristic, not a union of pipeline blobs.
Payloads are never concatenated. The bytes are rechecked before atomic staging
under `content/pipelines/`. `build_info.json.pipeline_caches` reports the selected
GPU, driver, shader/wgpu identity and size; `matches_engine` still refers to the
unsalted engine hash and can be false for a valid game-salted reference.

Invalid CRC/checksum/key, unsafe archive names, symlink cache inputs, unreadable
inputs and oversized files fail packaging. File reads are capped to the existing
512 MiB FPCH limit and 256 MiB feedback-ZIP limit. ZIP entries are never extracted
by their names. Compressed ZIPs and ZIP64 are unsupported; Regia feedback uses
stored entries. Repack third-party archives outside this tool if necessary.

Only package caches from trusted testers. Checksums detect corruption; they do
not authenticate driver payloads. Runtime seeding still requires an exact GPU,
driver, shader and wgpu key match and the driver's own validation. Other GPUs or
driver versions compile their own pipelines.

Verification: `fpc_from_bundle_merged_for_its_driver` exercises bundle collection,
largest selection, package staging and exact-driver seeding. The reference test
uses a real feedback bundle and rejects mismatched shader/wgpu pairs. The agent
capture/list test dispatches both methods and checks attachment and opt-out.
