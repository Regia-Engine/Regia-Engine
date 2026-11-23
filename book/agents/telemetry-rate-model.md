# Telemetry rate model

The telemetry crate exposes two small, allocation-free rate controls:

- `RateLimiter` admits the first event for a key, suppresses repeats during its window, and
  reports the suppressed count on the next admitted event. It has 64 slots; adding a new key when
  full replaces the slot whose current window opened earliest. Equal opening times resolve to the
  earliest slot.
- `TokenBucket` begins with `burst` tokens and adds one token per `refill` duration, up to the
  burst limit. `take` returns whether one token was available. Refusals accumulate in `refused`.

Both APIs accept a caller supplied `Instant`. A backward instant contributes zero elapsed time,
and a quiet gap cannot raise the bucket above its burst. A zero limiter window admits repeated
events immediately. The bucket uses a minimum effective refill interval of one millisecond.
Rate keys encode each part's byte length before its contents, so arbitrary byte values cannot
imitate a part separator. Per-key repeat counts and cumulative suppression/refusal counters
saturate at their integer maxima rather than wrapping.

These are Rust crate APIs, not agent methods. The `regia_agent` protocol does not expose calls or
error codes for them. The regression surface is the private reference-model suite in `rate.rs`:

```text
cargo test -p regia_telemetry rate::tests
```

The full crate suite also exercises the JSON logging layer and its allocation and span checks:

```text
cargo test -p regia_telemetry
```

The rate tests use bounded integer-time reference models and checked offsets from one anchor; they
do not sleep or assert timing thresholds. The JSON contract itself remains in
`docs/specs/28-log-contract.md`.
