# Destruction lab: repeated-session regression

The L2 lab is a single client app that owns a dedicated server and a second client. Its `LabSession`
also owns the script, queued local blasts, and the other client's app. A session ends when that host
app is dropped. Build each new session with `destruction_lab::lab_app` and a fresh `LabKit`; do not
reuse a `LabSession` between apps.

## Regression recipe

The recipe runs with the lab's original synthetic wall and tower kit. It does not use external
content, runtime fracture, or a reset method.

1. Create an app and queue `lab.fire` before its first fixed tick. Drop the app. Create another app,
   advance one tick, and call `lab.report`: both structures must be intact, explosion and broken-bond
   counts must be zero and the clients must agree. Inbox datagrams and gameplay messages must be
   empty; compare outbox datagram counts with a fresh control at the same fixed step because the
   runtime can emit normal unreliable datagrams. Do not compare opaque datagram bytes or transport ids.
2. Advance a scripted app through tick 75, while chunks are still in playback, then drop it. Run a
   second app with the alternate seed and fixed script for 75 ticks. Its report must equal a fresh
   control run with the same kit, script, and steps.
3. Run script A, script B with changed structure seeds, then A again, each for 75 ticks. Canonical
   reports for the two A runs must match. At every run, both clients must agree with each other and
   with the server's structure hash and explosion count.
4. The real agent protocol test creates two separate project directories. It calls `lab.fire` and
   `lab.report` on the first paused app, verifies the blast is still queued at tick zero, drops that
   app, then calls `lab.report` on the second app and checks its pristine baseline. The test
   synchronizes every call with a response channel while pumping app updates; it uses no sleep for
   synchronization.

Report comparison includes every `LabReport` field, including session ticks, counters, hashes,
structure ids, chunk poses, and playback state; no report field is normalized. Pending state checks
require empty inboxes, zero queued blast messages, and zero destruction stream items, and compare only
outbox datagram counts to an equal-step fresh control. They intentionally ignore opaque datagram bytes
and transport identifiers. Each protocol session uses
`regia_test_utils::temp::unique_temp_dir`; the test closes its client, drops the app, and removes the
unique project directory.

Run the bounded sample checks with:

```sh
cargo test -p destruction_lab --test acceptance
cargo test -p destruction_lab --test agent_dogfood agent_fire_and_report_are_scoped_to_their_endpoint_session
```

Existing `lab.fire` and `lab.report` coverage is listed in [COVERAGE](../COVERAGE.md) under the
destruction lab sample; this recipe adds cross-session verification without changing that protocol.
