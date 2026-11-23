# Media ring seek generation isolation

The ring's synchronization is an internal primitive. A private channel-scripted `FrameFeed` runs
through the same worker as `Session`; no codec, media file, GPU, timer, random input or new agent
method is involved. This regression verifies existing behavior in spec 50 sections 2.4 and 10.

Run the deterministic regression:

```sh
cargo test -p regia_media --lib ring::seek_generation_tests
```

The six tests check:

- Fill the ring, consume one picture to start another decode, issue two seeks while that decode
  waits on a test channel, then release it. Only the latest seek reaches the worker, with the
  accurate flag intact; no old picture or bytes reappear. Exact timestamps, feed statistics,
  queued frames/bytes, peak bytes, queued discards and empty nonblocking polls are checked.
- Let the worker apply one seek and begin decoding its generation before issuing another seek.
  Neither the original nor intermediate decode can be presented. Both seek modes are exercised.
- Release an old EOF or decode error after two seeks. Neither can terminate the new generation.
- EOF and errors remain terminal across blocking and nonblocking polls, without further decodes;
  a seek clears the ring's terminal latch and allows new pictures.
- A 12-byte picture is allowed alone in a 9-byte ring; subsequent 6-byte pictures obey the byte
  limit, independently of the frame cap. Peak bytes remain 12 after draining.
- Drop the consumer with a worker waiting at the frame limit, byte limit, or oversized singleton
  limit. Retain and join the actual worker handle and check its completion state and feed-drop
  acknowledgement. The same completion check runs after EOF/error. A bounded Drop returning
  without its worker finishing cannot pass this assertion.

`dropped_by_seek` counts queued entries discarded by a seek. The existing worker discards an
in-flight result without adding to this counter; the tests preserve that distinction. EOF/error
entries occupy a queue slot and contribute zero bytes. The worker idles after a terminal result
until seek or consumer drop; EOF itself does not destroy the worker.
Post-error seek uses the scripted feed to isolate the ring's latch reset. A failed `Session` or
player retains its own failure behavior.

For the existing host surface, use the [media renderer recipe](media-render.md) to queue
`media.seek` and inspect `media.state`. These calls expose frame time and ring counters; they do
not force a decode interleaving. The existing `regia_agent` tests exercise their normal happy and
error paths:

```sh
cargo test -p regia_agent --lib media::tests:: -- --test-threads=1
```

`controls_round_trip_through_the_source` verifies a seek to 0.4 seconds and its published
`frame_time`; `error_reasons` checks `media_player_not_found`, `media_control_queue_full` and
`media_not_installed`. They are lowercase `data.reason` values on `F0002` errors. Missing or
invalid `time` remains an invalid-parameters error. No new error code is introduced here.

The oracle uses literal expected timestamps and plane lengths, rather than reproducing the
worker's generation or room calculation. For a mutation check, temporarily bypass only the
worker's stale-result rejection and run
`two_seeks_during_one_decode_coalesce_and_never_present_the_old_generation`: it must fail when
the old result reappears in the queue. Restore the rejection and rerun the complete six-test
regression before committing. Do not run the mutation as a supported configuration.

On integration base `b81b90c3405ab5974f2694e5a0cbc5525d4e1ec6`, all six cases passed without a
worker behavior change. Temporarily replacing the stale-result guard with `if false` made the
coalesced-seek test fail with one queued frame and 6 bytes where it expected zero. Restoring the
guard returned all six cases to green. This demonstrates missing coverage rather than a newly
reproduced ring defect.
