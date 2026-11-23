# Replay counter limits (agents)

The existing sealed link uses a 64-bit header field with the high byte reserved for the rekey epoch. Its send counter is limited to
`(1 << 56) - 1`; that reserved final counter value is never sealed. `SecureLink::seal` returns `CryptoError::Exhausted` at the
limit and continues returning it, without wrapping or reusing a nonce. There is no reconnect, reset, or counter extension behavior
at exhaustion: the caller must end the link through its existing session lifecycle.

On receive, `SecureLink::open` rejects counters at or above that limit. It checks freshness before decrypting, then advances the
replay window and receive-key epoch only after authentication succeeds. A forged high counter therefore cannot advance the window;
the authentic earlier record remains usable. Duplicate authenticated records return `CryptoError::Replay`.

## Existing calls and errors

No agent method or transport call was added for this boundary. Existing callers continue to use the sealed-link path:

- `SecureLink::seal(kind, body)` returns a sealed datagram or an existing `CryptoError`, including `Exhausted` at sender limit.
- `SecureLink::open(header)` returns the decoded kind and body or an existing `CryptoError`, including `Replay` for stale, duplicate,
  or out-of-range counters and `Auth` for failed authentication.
- Existing `NetClient` and `NetServer` session handling consumes these results and keeps its established disconnect/error behavior.
  Query `net.crypto` through the existing `regia_agent` call to inspect crypto totals; it does not expose or reset per-link counters.

The deterministic regression coverage is exercised by:

```text
cargo test -p regia_net --lib crypto
cargo test -p regia_net --test crypto --test crypto_fuzz
cargo test -p regia_net
```

The replay-window set oracle uses subtraction with checked arithmetic, so the model itself stays valid at `u64::MAX`. Its fixed traces
cover word boundaries, the `REPLAY_WINDOW_COUNTERS` edge, large jumps, unseen backward counters, duplicates, and repeated freshness
checks before commit. Sealed-link unit tests cover sender exhaustion, high-counter authentication failure, and preservation of the old
receive state.
