# Android asset pack row contract

`regia_platform::asset_packs::PackState::from_row` decodes the four values returned by the Android
activity bridge: status, error code, downloaded bytes, and total bytes. A row must contain at least
four values; shorter rows have no state. Extra values are ignored. Status and error values that fit
in `i32` pass through, including unknown status codes. A status or error outside that range maps to
`-1`. Negative byte counts map to zero; nonnegative counts are preserved as `u64`, including a
downloaded count greater than the reported total.

This is bridge decoding only. Content-layer validation remains responsible for interpreting a store
status (`store_unknown_status` for an unrecognized value), verifying downloaded pak content
(`hash_mismatch`), and reporting source failures (`source_unreachable`). The bridge does not clamp
downloaded bytes to total bytes, infer a content state, or validate a pack name.

On desktop, the six existing entry points (`pack_state`, `fetch_pack`, `cancel_pack`, `remove_pack`,
`pack_location`, and `show_confirmation`) immediately return
`ServiceError::Unsupported("store asset packs are Android-only")`. Their desktop cfg branches do
not call a platform service or access files.

## Existing checks and calls

Run the focused parser and desktop checks with:

```sh
cargo test -p regia_platform --lib asset_packs
```

The existing content-agent path for inspecting and requesting a pack is:

```sh
regia agent call content.chunks --project .
regia agent call content.request '{"chunks":["<chunk>"],"mount_when_done":true}' --project .
regia agent call content.progress '{"request":1}' --project .
```

`content.chunks` reports the `android_pack` source; progress may include `Downloading`,
`Verifying`, then `Mounted`. The established content failures are `store_unknown_status`,
`hash_mismatch`, and `source_unreachable`. These agent methods predate this page; this contract adds
no agent method or error code.

The desktop test proves all six calls return `Unsupported`. The absence of IO follows from each
non-Android function branch directly returning that error without invoking a platform function; no
filesystem access is part of this test.
