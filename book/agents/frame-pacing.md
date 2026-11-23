# Frame pacing

On an Android phone each frame starts on a display vsync from `AChoreographer` and stays on screen for
the same whole number of vsyncs, the swap interval. That keeps frames even and stops them piling up
in the swapchain queue, which would add latency (ADR-1942). Other platforms pace with the software cap
(`FramePacing::target_fps`) and the swapchain. The pacing model itself is plain code, so you can run it
on any platform without a phone.

| Method | Permission | What it does |
|---|---|---|
| `pacing.get` | read | The mode, what paces frames now, the panel rate, the swap interval and the late frames. |
| `pacing.set` | play | Pick `auto`, `off` or a forced `interval`. |
| `pacing.simulate` | read | Run the pacing model on virtual time, on any platform. |
| `pacing.trace` | play | The last frames as the pacer saw them: cost, swapchain blocking, tick wait, interval. |

## Read what the phone does

`pacing.get` answers:

```json
{
  "mode": "auto", "interval": null, "cost": "work", "hint": "double",
  "source": "choreographer", "available": true, "ticks": 5400,
  "refresh_hz": 90.0, "swap_interval": 2, "slot_ms": 22.2,
  "frames": 1200, "late": 14, "late_pct": 1.2, "held": 0, "probes": 0, "timeouts": 0,
  "interval_changes": 1, "refresh_changes": 0,
  "frame_rate_hint": null, "max_fps": null
}
```

- `source` is `choreographer` while frames wait for vsync ticks, and `software` otherwise: on a desktop,
  before the first few ticks, with mode `off`, or after a stall.
- `available` says whether the platform delivered any ticks at all.
- `swap_interval` and `slot_ms` are `null` on the software path. `slot_ms` is how long each frame stays
  on screen.
- `late` counts frames that started a vsync or more after their slot. `late_pct` is `null` before the
  first paced frame. Both count from the app's start: read them before and after a window to measure
  that window, as `cargo xtask android-pass` does for its `pacing_late` budget.
- `hint` is the rate policy for the compositor (see "The rate hint" below): `double`, `slot` or `off`.
- `probes` counts the times `auto` tried a shorter slot that its frames did not fit on paper (see
  "When the cost grows with the slot" below).
- `held` counts frames the swapchain held back: their acquire or present blocked for more than an eighth
  of a vsync because every buffer was queued or still on screen. The display queue set their start, so
  they are not counted as late. A frame like that which has missed its tick starts at once.
- `timeouts` counts waits that ended without a tick. They mean the display stopped ticking. After three
  in a row, frames return to the software cap until a tick arrives.
- `frame_rate_hint` is the rate suggested to the compositor (`ANativeWindow_setFrameRate`, API 30), only
  while the app runs below the panel rate and `hint` allows one. A panel may follow it, and then
  `refresh_hz` shows the slower rate and `swap_interval` reads 2 or more under the default `double` hint
  (1 under `slot`) while the hint stays. It clears once frames have fit a shorter slot for
  a while, and the panel comes back.

## Choose a mode

`pacing.set { "mode": "auto" }` is the default: the interval follows the frame cost. `{ "mode": "off" }`
leaves frames to the software cap and the swapchain, the path every build used before. `{ "mode":
"interval", "interval": 2 }` forces 2 vsyncs per frame (1 to 8), which is useful for comparing
frame times at a fixed cadence. It answers `{ before, after }`, two `pacing.get` answers. On a desktop
the mode is stored and changes nothing. An unknown mode, or an `interval` outside 1..=8 or without mode
`interval`, is refused with the usual parameter error `F0002`.

`cost` picks what `auto` counts as a frame's cost when it chooses the interval (ADR-2320). `work`, the
default, is the frame's time less the time it sat blocked in the swapchain (`get_current_texture` and
`present`). `frame` is the whole time, as v0.2.10 counted it. The swapchain blocks while the display
queue drains at the rate pacing chose, so with `frame` a slow slot makes every frame look slow and the
interval climbs to the 20 fps limit and stays there. You can send `cost` alone (`{ "cost": "frame" }`)
or with a mode; `pacing.get` reports it. Any other value is refused with `F0002`.

## The rate hint

`hint` picks what `frame_rate_hint` asks of the compositor while the slot is longer than one vsync
(ADR-2339). `double`, the default, asks for the slowest rate on the panel's grid that still gives each
frame two vsyncs or more: on a 60 Hz panel a 33.3 or 50 ms slot asks for nothing and the swap interval
paces the frames; on 120 Hz a 33.3 ms slot asks for 60 and a 50 ms slot for 40. `slot` asks for the
slot's own rate, one vsync a frame, as v0.2.10 did. `off` never asks. Send it alone
(`{ "hint": "slot" }`) or with a mode or a cost; any other value is refused with `F0002`.

Why two vsyncs: a phone's display queue holds two of the app's buffers, so a frame needs about two
latches of the compositor, and on Android the wait for them shows up inside the frame as GPU and CPU time,
not in acquire. On the Fairphone 5 every setting ran at about half the panel's rate: 34 fps with pacing
off at 60 Hz, 14 fps with a 30 Hz hint, 10 with 20 Hz. The `slot` hint therefore halved whatever rate the
interval had chosen, and the interval kept rising.

## Read the frames one by one

`pacing.trace { "frames": 240 }` returns the newest frames the pacer handled, oldest first (up to 1024,
default 120), and medians:

<!-- not-a-request -->
```json
{
  "log": false,
  "summary": { "frames": 240, "fps": 44.9, "paced": 240, "gap_p50_ms": 22.2,
               "cost_p50_ms": 14.1, "swapchain_wait_p50_ms": 1.2, "work_p50_ms": 12.8,
               "waited_p50_ms": 8.0 },
  "rows": [{ "start_ms": 0.0, "cost_ms": 14.3, "swapchain_wait_ms": 1.1, "work_ms": 13.2,
             "waited_ms": 7.9, "release_tick_ms": 22.2, "ticks": 2, "period_ms": 11.1,
             "interval": 2, "hint_hz": 45.0, "paced": true }]
}
```

- Times in `rows` are ms from the first row returned. `start_ms` is when the frame started, `cost_ms`
  how long it ran, `swapchain_wait_ms` the part blocked in the swapchain, `work_ms` the difference,
  `waited_ms` the wait for a vsync tick afterwards, and `release_tick_ms` the tick that released the
  next frame (`null` when the frame was not paced or the wait timed out).
- `ticks` counts vsync ticks that arrived during the frame and its wait: on a panel that follows the
  rate hint it drops with the hint.
- `paced: false` rows are frames on the software path (mode `off`, a hidden window, a stall). They still
  show the cost and the swapchain blocking, which is how you compare `auto` with `off`.
- `log: true` also writes one line per frame to the log (target `regia_pacing`, level info, so logcat on
  Android); `log: false` stops it. `clear: true` forgets the rows after the answer, to start a clean
  window. `frames` outside 1..=1024 and a non-boolean `log` or `clear` are refused with `F0002`.
- Only a platform with a vsync tick source fills the trace. On a desktop it stays empty.

### Ticks apart from the frames (ADR-4913)

A low frame rate has two different causes that look the same from outside: the vsync ticks
themselves arrive late or not at all, or the ticks keep time and the frames are slow to use them
(work, a full display queue, a long slot, the software cap, the event loop). Each row and the
summary separate them:

- Per row, `tick_gap_max_ms` is the longest gap between two ticks the pacer received during that
  frame, and `missed_ticks` counts whole refresh periods missing between received ticks. A tick
  source that keeps time shows one period and 0, however slow the frame is. The gap across a
  suspend is not counted. `cap_waited_ms` is the software cap's sleep after a frame the display did
  not pace (mode `off`).
- `summary.breakdown` (`null` with fewer than two rows) splits each interval from one frame start to
  the next into `work`, `swapchain`, `wait` (the pacer holding the slot), `cap` and `loop` (time
  outside the frame: the event loop). It gives medians (`*_p50_ms`), `shares` that add up to 1,
  `vsyncs_per_frame`, `ticks`, `missed_ticks`, `tick_gap_max_ms` and `limit`:

<!-- not-a-request -->
```json
{ "intervals": 239, "interval_p50_ms": 66.6, "work_p50_ms": 9.8, "swapchain_p50_ms": 0.6,
  "wait_p50_ms": 55.4, "cap_p50_ms": 0.0, "loop_p50_ms": 0.4,
  "shares": { "work": 0.15, "swapchain": 0.01, "wait": 0.83, "cap": 0.0, "loop": 0.01 },
  "period_ms": 16.7, "vsyncs_per_frame": 4.0, "ticks": 956, "missed_ticks": 0,
  "tick_gap_max_ms": 16.9, "limit": "wait" }
```

`limit` is `ticks` when more than an eighth of the expected vsyncs never arrived; otherwise the largest
share. Read it this way: `ticks` is a tick-source problem (the vsync thread, the compositor); `wait` with
little work means the slot is longer than the frames need; `swapchain` is back-pressure from the
display queue; `work` is the frame itself; `cap` is the software cap; `loop` is time between frames. To
compare `auto` with `off`, take one trace in each mode with `clear: true` between them, e.g.:

<!-- not-a-request -->
```sh
regia agent call pacing.set '{"mode":"off"}'
regia agent call pacing.trace '{"clear":true}'
# F = the current Time.frame plus 300 (read it with play.expect or world.query first)
regia agent call play.wait_until '{"condition":{"resource":"Time","path":"frame","op":"gte","value":F},"timeout_ms":30000}'
regia agent call pacing.trace '{"frames":240}'
```

Wait on frames, not on a sleep: a phone that runs slower than you expect then still fills the window.

A frame that ends after its slot's tick starts at once on that tick with `cost: "work"` (the default). It does
not wait for the next tick: on a 60 Hz panel with a three-vsync slot, a frame 3 ms over its 50 ms slot used to
idle until 66.7 ms (ADR-4913). Such a frame still counts as over budget, so a run of them raises the interval as
before. `cost: "frame"` keeps the older wait.

## Try the model without a phone

`pacing.simulate` feeds the model an even vsync train and the frame costs you give, in virtual time.
It needs no window and takes no wall-clock time, so a result is the same on every machine:

```json
{ "method": "pacing.simulate", "params": { "refresh_hz": 60, "frame_ms": [20], "frames": 600 } }
```

<!-- not-a-request -->
```json
{
  "period_ms": 16.67, "swap_interval": 2,
  "steps": [{ "frame": 3, "interval": 2 }],
  "frames": 600, "late": 2, "interval_changes": 1, "refresh_changes": 0,
  "tail_min_gap_ms": 33.33, "tail_max_gap_ms": 33.33, "tail_late": 0,
  "frame_rate_hint": null
}
```

- `frame_ms` is one cost or a list repeated for the run (up to 4096 entries, each 0 to 10000 ms), which is
  how you describe a scene that alternates. A run past a million vsyncs is refused.
- `frames` is 1..=20000, default 600. `refresh_hz` and `switch_hz` are 5..=500; `refresh_hz` defaults to 60.
- `cap_fps` adds the software cap. `pacing.simulate` accepts 1..=1000; a higher value is refused before the handler runs. `interval` forces an interval.
- `switch_frame` with `switch_hz` changes the panel's rate during the run.
- `follow_hint: true` makes the stand-in compositor honor the rate hint, as a phone with frame rate
  control does: the ticks slow to a whole divisor of the panel's rate and return when the hint clears.
  A healthy run shows `refresh_changes` of 1 and no more. `frame_rate_hint` in the answer is the hint at
  the end of the run.
- `tail_*` covers the last quarter of the run, after the interval settled. Equal `tail_min_gap_ms` and
  `tail_max_gap_ms` mean perfectly even frames; `tail_mean_gap_ms` is the average, so 1000 / it is the fps.
- `swapchain_images` (2..=8) adds a stand-in swapchain: the GPU runs presented frames in order, `gpu_ms`
  each (0..=10000), and the compositor latches the oldest finished frame `latch_ms` after each tick
  (0..=1000). The buffer it replaces stays with the display until the next latch. A frame blocks at its
  start while every buffer is queued or with the display, as a phone's acquire does.
  `tail_swapchain_wait_ms` is the mean blocking and `held` counts the held frames. `gpu_ms` or `latch_ms`
  without `swapchain_images` is refused.
- `cost` is `work` (default) or `frame`, as in `pacing.set`. Without a swapchain the two costs
  see the same frame costs and neither blocks, but they are not the same run: `work` starts a
  frame that missed its slot's tick at once (ADR-4913), so the reports differ once frames end off
  the tick grid (the first frames of a run, an overrun at a probe's short slot).
- `hint` is `double` (default), `slot` or `off`, as in `pacing.set`; it only matters with
  `follow_hint: true` or for the `frame_rate_hint` in the answer.
- `stretch` (0..=0.95, default 0) makes each frame's work grow into that fraction of the idle time of
  recent frames: a frame of `w` ms, when frames recently started `g` ms apart (a running mean over about
  8 frames), costs `w + stretch * (g - w)`. It stands in for a phone whose clocks fall when paced frames
  leave it idle. Anything outside the range is refused with `F0002`. `probes` in the answer counts the
  probes.

A phone like the two test phones, with two buffers and a compositor that follows the hint:

```json
{ "method": "pacing.simulate", "params": { "refresh_hz": 60, "frame_ms": [6], "frames": 2000,
  "follow_hint": true, "swapchain_images": 2, "gpu_ms": 7, "latch_ms": 4, "cost": "frame" } }
```

With `cost: frame` (the v0.2.10 model) this settles on `tail_mean_gap_ms` 100 and a 20 Hz hint: 10 fps,
as both phones measured. With `cost: work` it runs at 33.3 ms with no hint and no late frames, which is
what the two buffers allow and what the phones measured with pacing off.

## When the cost grows with the slot

On the Fairphone 5 a frame took 29 ms with pacing off and 51 ms at the 15 Hz slot `auto` had settled on,
with acquire and present under 2 ms either way: the cost grows with the slot (ADR-2339). A cost like
that never fits a shorter slot on paper, so after a start-up stall raised the slot it never came back.
With `cost: work`, `auto` now probes: when the slot was raised by a frame at least a third costlier than
the current frames and they still do not fit the next shorter slot, it tries that slot after a hold anyway.
For up to 120 frames, or until 8 in a row fit, a frame that misses its tick starts at once (late frames
are still counted) and over-budget frames do not raise the slot. If the frames fit after that, the slot
stays. If not, the slot goes back to where the probe started, the next hold doubles, and that raise
becomes the reference, so a frame cost that really needs the slow slot is probed once. `pacing.get`
counts `probes`; `cost: frame` never probes.

A change of refresh rate can start such a probe too (ADR-5946). The slot is kept across the change, and
a slot that lies between two counts of the new ticks used to become the nearer one: 25 ms (three ticks
of 120 Hz) became 33 ms when the ticks slowed to 60 Hz. The TEST Pixel 9 Pro gives a new process 120 Hz
ticks for its first second or two, so a stall in the first frames left the app at 30 fps, where its
frames cost 15 ms instead of 9: too much to count as fitting one tick, too little for the probe above.
Now a change that would round the slot up takes the shorter slot as a probe (`probes` counts it) and
goes back to the longer one only if the frames do not fit. In `pacing.get`, `refresh_changes` 1 with
`swap_interval` 1 and `probes` 1 a few seconds after a start is this.

To try it, send `pacing.simulate` with `follow_hint: true`, `stretch: 0.6` and a `frame_ms` list of ten
300 ms start-up frames followed by 2990 entries of 24 ms (the list repeats, so list the light frames out
for a single stall), `frames: 3000`. `cost: frame` ends at 100 ms per frame (every frame late, 10 fps, what
the Fairphone measured); `cost: work` ends at 33.3 ms after one or two probes (25 ms at
120 Hz, where the late start's shorter gaps shrink the growing cost until the 25 ms slot fits).

## What to expect

| Panel | Frame cost | Settles on |
|---|---|---|
| 60 Hz | 8 ms | 1 vsync, 60 fps |
| 60 Hz | 20 ms | 2 vsyncs, 30 fps |
| 90 Hz | 20 ms | 2 vsyncs, 45 fps |
| 120 Hz | 20 ms | 3 vsyncs, 40 fps |
| 60 Hz | 70 ms | 3 vsyncs, the 20 fps limit; frames keep running late and are counted |
| 240 Hz | 40 ms | 10 vsyncs, about 24 fps: inside the 50 ms limit, past a forced interval of 8 |
| 60 Hz, 45 fps cap | 2 ms | 2 vsyncs, 30 fps: a cap is never exceeded |

The 20 fps limit is a 50 ms slot, so the vsync count rises with the refresh rate. It is not capped at 8. A forced `interval` is still 1..=8. A refresh change keeps that slot's duration: 3 vsyncs at 60 Hz (50 ms) stay 12 vsyncs at 240 Hz.

Frame cost here is the frame's work (`cost: work`), not its swapchain blocking.
An interval rises after about four frames over budget, or when about a third of the frames run over.
A rare spike leaves it alone. It falls after 60 frames fit a lower interval with 12.5 % to spare, straight to the shortest slot
the costliest of those frames fits, and that wait doubles each time a drop bounces back, so a scene that
alternates does not flap.

## When a frame starts late (ADR-5461)

A frame paced on the display starts when its release tick has reached the app and the frame loop is
running again. Each `pacing.trace` row has two stamps for that path, on the same origin as `start_ms`:

- `tick_delivered_ms`: when the release tick's Choreographer callback ran on the vsync thread. Less
  `release_tick_ms` (the vsync's own time), it is the compositor's delivery delay. `null` for a frame
  that was not paced, and on a tick source that does not stamp delivery.
- `woke_ms`: when the frame loop came back from waiting for that tick. Less `tick_delivered_ms` it is
  the hand-off to the frame loop. The next row's `start_ms` less `woke_ms` is the event loop's own time.

On the TEST Pixel 9 Pro (60 Hz) delivery is steady (p50 0.5 ms, p99 under 1 ms) and the event loop takes
about 0.1 ms, but a frame loop that slept until the tick sometimes took a millisecond or more to run
again: about one frame in ten started 1.03 to 1.07 refresh intervals after the one before. That was the
`presented_p99` tail of `cargo xtask android-pass`. A timed sleep was worse: it woke about 1.7 ms late at
the median.

The fix is the **wake-ahead**, on by default. The pacer predicts when its release tick is due (the tick on
the period grid, plus a running mean of the measured delivery delay). The frame loop sleeps until
`wake_ahead_ms` before that, then polls for the tick, so it is already running when the tick arrives. The
poll ends `wake_ahead_ms` after the due time at the latest (a clock that stands still, as a test's stand-in
may, ends it after twice that on the wall clock), and then the loop sleeps for the tick as before. It costs up to about 2.5 ms of polling per frame on one
core. If a tick's callback is more than 0.6 ms past its usual delivery (the vsync thread woke late), the poll
releases the frame on the predicted tick. That only happens for a tick one slot past the newest real tick,
so a display that stops ticking still stalls as before. `pacing.get` reports `wake_ahead_ms` (`null` when
off), `wake_ahead_polled` (frames whose tick the poll caught) and `wake_ahead_predicted` (frames released on a
predicted tick). A row released on a predicted tick has `tick_delivered_ms` `null`.

- `pacing.set { "wake_ahead_ms": 2.5 }` sets the lead, 0 to 4 ms. `0` or `null` turns the wake-ahead off,
  and anything outside the range is refused with `F0002`.
- `pacing.set { "perf_hint": true }` opens a performance hint session (Android API 33 and newer,
  `APerformanceHint`). The slot is its target and each paced frame's work is reported to it, for the frame
  loop and the vsync thread. It is off by default because it did not shorten the late starts on the test
  phone. `pacing.get` `perf_hint` reports `{ enabled, active, unavailable, target_ms, reports, failures }`.
  `unavailable` means the platform gave no session (an older phone, or one without the API), and `null`
  means no runner keeps one (a desktop test world).

To find where a late start came from, read a window of rows and split each long interval into delivery,
hand-off and event loop. A frame that ran past its slot shows `waited_ms` 0 instead: its own work or the
swapchain held it, and no wake-up setting helps it.

## When the display queue stays full

After a burst of frames (pacing `off`, a preset change, a hitch) the swapchain can hold one image more than the slot
needs. Every frame then blocks in acquire or present for most of a vsync, starts at once on the tick it missed, and
fills the queue again. `pacing.trace` shows it: `waited_ms` 0 on every row and a large `swapchain_wait_ms` (about
8 ms at 60 Hz on the TEST Pixel 9 Pro, against 1-3 ms normally), and the frame starts jitter with the compositor's
buffer releases (presented p99 1.25-1.29 x the refresh there).

With `cost: work` (the default) the pacer drains such a queue: after 2 held frames in a row that started at once on a
missed tick (`drain_run`; 8 before ADR-5946), the next frame waits for the following tick, so the last image stays
one vsync longer and the queue is one image emptier. Held frames that still wait for their slot do not count. If the queue fills again at once (the
frames really are limited by the display queue), the next drain needs twice the run, up to 1024 frames. `pacing.get`
counts `drains`. `cost: frame` and `pacing.simulate` do not drain (ADR-5564).

## Steady frame starts (ADR-5946)

With the wake-ahead a frame still started when its tick's callback was *seen*, and Choreographer delivers a callback
0.3 to 1.0 ms after the tick's own time on the TEST Pixel 9 Pro. Two starts in a row were then up to 0.7 ms further
apart than their ticks: a frame interval p95 of 16.9-17.0 ms and a p99 of 17.05-17.30 ms on a 16.66 ms refresh, with
nothing wrong. `presented_p99` is the 6th longest of the 480 intervals of its 8 s window, so two or three real
outliers on top of that tail put it at its 1.05 x limit.

A paced frame now starts **on the grid**: when its tick is due at the frame loop (the tick's time on the grid of the
real ticks plus the usual delivery delay), whether the callback has run or not, and never before. The callbacks still
set the grid. A tick is only predicted one slot past the newest real one, so a display that stops ticking stops the
frames as before. Same phone, same session, interleaved 12 s windows: presented p99 1.008-1.010 against 1.023-1.034,
interval p95 16.71 ms and p99 16.75-16.80 ms.

`pacing.get` reports `steady_start: { release, grid_releases, grid_follow, start_offset_ms, drain_run }` and
`grid_moves`; `pacing.set` takes the four settings:

- `release`: `grid` (default) or `tick`, which starts the frame when the callback is seen (ADR-5461), for a
  comparison on the same build. `grid_releases` counts the frames released on the grid.
- `grid_follow`: `true` (default). A panel that runs faster than the rate it gives the app (this phone: 120 Hz
  panel, 60 Hz ticks) moves the app's ticks by one panel vsync now and then. The slot of the frame in flight was
  counted from the old grid, and by a few microseconds either way the next frame waited or started at once as one
  frame too many, which left the display queue one image fuller. With the follow, a tick that is not a whole number of
  periods after the last one moves the slot onto the new grid. If the last frame started on a real tick, the moved
  tick starts the next one. If it started ahead of its callback (on a predicted tick), the moved tick is that
  frame's own. Either way: one interval of a period and a half, no extra frame, no skipped vsync. `grid_moves`
  counts them.
- `drain_run`: 1 to 1024, default 2: the held late starts in a row that make the next frame wait a vsync (see "When
  the display queue stays full"). On the phone, pacing `off` then `auto` took 6-7 held frames and 2-4 long intervals
  to get back on the slot with a run of 8, and 0-1 held frames and 1-2 long intervals with 2.
- `start_offset_ms`: -8 to 8, default 0: where a frame released on the grid starts against its tick's due time. The
  compositor wakes for its own frame at the app's tick. When the app's frames are latched one compositor cycle late
  (below), a frame that starts right then asks for its swapchain image while the compositor is still handing the last
  one back, and waits for it. On the phone `3` and `5` took that wait away (acquire plus present median 1.75 -> 0.70 ms,
  p90 3.3 -> 0.9 ms), and a negative offset lengthened it by as much. It is 0 by default: a later start leaves less
  time before the latch after next (one of three windows at `5` had a stuffed queue and 12 long intervals), and the
  right value depends on the phone's compositor. On the final build `3` left 40-48 of 1,035 frames held in the
  swapchain instead of 488-515 (`held` in `pacing.get`) with no interval over 1.05 x in three windows, and 4-6 starts
  per window were late instead of 2-3 (`late`: the frame before ran past its slot's tick); `4`: 1-4 held, 8-15 late.

### Where a long interval came from

Each `pacing.trace` row says how its frame was released and what the scheduler did to the frame loop meanwhile, and the
answer's `origin_ns` is the rows' time origin on the ticks' clock (`CLOCK_MONOTONIC` on Android), so a row can be lined
up with `dumpsys SurfaceFlinger` or a perfetto trace:

- `release`: `grid`, `polled`, `predicted` (ADR-5461), or `slept` (the tick was there when the wait began, or the wait
  fell back to sleeping for it).
- `sleep_late_ms`: how late the timed sleep before the poll came back. More than `wake_ahead_ms` is a late start.
- `work_run_delay_ms`, `wait_run_delay_ms`: time the frame loop thread was ready to run but had no CPU, during the
  frame's work and during its wait (`/proc/thread-self/schedstat`). `preemptions`: times the scheduler took the CPU
  away. `wake_cpu`: the CPU it was on when the wait returned. All `null` where the platform has no such counters
  (anything but Android).

On the TEST Pixel 9 Pro the sleep came back 0.11 ms late at the median and 0.18 ms at p99, and the run delay during the
wait was 0.05 ms at p99: the scheduler was not the cause there. Nice values of -4 and -10 for the frame loop and vsync
threads were tried and changed nothing, so the engine leaves thread priorities alone.

What is left on that phone are single frames held 3-8 ms in the swapchain acquire (`waited_ms` 0 and a large
`swapchain_wait_ms`), one to five in 12 s. The phone's build has no kernel tracing, but a perfetto trace with the
`android.surfaceflinger.frametimeline` and `track_event` data sources shows the compositor's side: its wake-up timer
fires together with the app's tick, the layer's pending-buffer counter (`BufferTX`) never returns to 0 (the frame
needs about 20 ms from tick to GPU done, so it is always latched one cycle late), and every frame is composited on
the GPU because the app's landscape buffer is rotated by the compositor (`dumpsys SurfaceFlinger`: `ROT_90`,
`CLIENT`). A swapchain of 4 or 5 images did not change any of it.

## Paused scene hold (ADR-7890)

A paused game often shows a menu over a world that does not move. Rendering that world at full cost every frame
spends the whole GPU budget on an image that does not change. The scene hold keeps one scene frame and redraws only
the UI over it.

| Method | Permission | What it does |
|---|---|---|
| `render.scene_hold` | read | Whether the hold is on, and per window the newest frame's step, scene passes and held GPU time. |
| `render.set_scene_hold` | play | `{ on }` turns the hold on or off; `{ refresh: true }` renders one more scene frame and holds that. |

How it works:

- When the hold turns on, the next frame renders the scene as usual (`step: "capture"`) and keeps a copy of the
  window's composited scene image: after post and upscale, before the UI pass.
- Every later frame is `step: "held"`. No scene pass runs: no uploads, shadows, GI, terrain, VFX or post. The kept
  image is written back, the UI is drawn over it, and the frame is presented.
- A resize, a new draw format (an HDR toggle), a lost and recreated surface, or `refresh: true` renders one scene
  frame and holds that one.
- Turning the hold off renders full frames from the next frame. The renderer keeps its temporal history: a held
  frame does not run the renderer, so the next scene frame reprojects from the last one as after one long frame.
- Frame generation does not run while the hold is on.
- A window whose swapchain image cannot be copied (no `COPY_SRC`) renders full frames.
- `{ follow_pause: true }` makes the hold follow the gameplay pause: it is on while `Time::paused` is set and off
  when the game unpauses, whatever `on` says (ADR-10105). `render.scene_hold` then reports `follow_pause: true` and
  `paused`. See [Gameplay pause freeze](game-pause-freeze.md).

```sh
regia agent call render.set_scene_hold '{"on": true}' --project <project>
regia agent call render.scene_hold --project <project>
```

`render.scene_hold` answers:

<!-- not-a-request -->
```json
{
  "on": true, "refreshes": 0, "follow_pause": false, "paused": true,
  "last_frame": { "step": "held", "held": true, "scene_passes": 0, "gpu_ms": 0.31,
                  "gpu_passes": [ { "pass": "scene hold composite", "ms": 0.05 }, { "pass": "ui", "ms": 0.26 } ] },
  "windows": [ { "window": 4294967297, "step": "held", "held": true, "scene_passes": 0, "gpu_ms": 0.31,
                 "gpu_passes": [ ... ], "held_frames": 1520, "captures": 1, "full_frames": 8800 } ]
}
```

- `gpu_ms` and the pass times are `null` until a held frame's timestamps resolve, and always on a device without
  timestamp queries. They are the composite plus the UI and the display stage.
- `scene_passes` is the scene views the frame rendered: 0 while held.

In a game, set `GameFlowSettings { hold_scene_when_paused: true, .. }` and the hold follows `GameFlow::Paused`. In
Rust, `regia_render::SceneHold::set(true)` on the resource; a tool that renders offscreen uses
`regia_render::RetainedScene` (`plan`, `record`, `finish_ui`, `after_submit`) itself.

Telemetry counts held frames apart. A held frame is not one of a `regia::frame_summary` window's `frames`; the summary's
`held` section has `frames` and `gpu_ms_mean`. Held frames never feed the hitch median or the frame-time histograms,
and `telemetry.live` says `held: true`, so a frame-time governor can skip them. A held frame that is still a hitch
(a stall while paused) is logged with `held: true`, `scene_passes: 0` and the held GPU time.

Errors: `render.scene_hold` and `render.set_scene_hold` answer `F0003` (`not_found`) when the app has no
`SceneHold` (no render plugin), and `render.set_scene_hold` answers `F0002` (`invalid_params`) when neither `on` nor
`refresh` is given or either is not a boolean.

## Caps per app state (ADR-7889)

A title menu, a loading page or a paused scene costs little, so without a cap it runs at hundreds of frames per
second and keeps the GPU busy for nothing. The host sets `regia_app::PacingCaps { play, paused, menu, loading }`
(each `Option<f32>` frames per second, `None` by default) and the `regia_app::PacingState` it is in. With
`regia_framework`'s `GameFlowPlugin` every flow transition sets the state: MainMenu is `menu`, Boot and Loading are
`loading`, Paused is `paused` and Playing is `play`.

The cap that paces is the tighter of the state's cap and the player's cap (`FramePacing::target_fps`, which the
settings apply writes from `display.frame_cap`). The settings apply only writes the player's cap, so a settings
change never removes a menu cap, and a menu cap never raises a lower player cap. The cap applies on every runner
path the player's cap applies to: with vsync off, on the display pacer and with frame generation on (there it paces
rendered frames; generated frames are presented between them while the rendered rate stays above frame generation's
55 fps floor). With no cap set anywhere nothing changes.

| Method | Permission | What it does |
|---|---|---|
| `app.pacing` | read | The pacing state, the cap of each state, the player's cap, the cap that paces and the measured frame rate. |
| `app.set_pacing_state` | play | Set the pacing state, the caps, or both. |
| `render.dynres` | read | The dynamic-resolution governor: permille, `held`, bound, median GPU time. |

`app.pacing` answers:

<!-- not-a-request -->
```json
{ "state": "menu", "caps": { "play": null, "paused": 30.0, "menu": 60.0, "loading": 60.0 },
  "player_fps": 144.0, "state_fps": 60.0, "effective_fps": 60.0, "period_ms": 16.67,
  "paces": true, "loop_mode": "continuous", "measured_fps": 59.9 }
```

- `player_fps` is `FramePacing::target_fps`; `state_fps` the current state's cap; `effective_fps` the tighter of
  the two (`null`: uncapped or vsync only), and `period_ms` its frame period.
- `paces` is false when the loop mode skips the cap (`bench`, or a frozen `run_frames` loop, which runs at the
  throttled rate instead).
- `measured_fps` is the diagnostics' rolling rate, `null` without the diagnostics plugin.

`app.set_pacing_state { "state": "menu", "caps": { "menu": 60, "loading": 60, "paused": 30 } }` sets both (either
one alone works too) and answers `{ before, after }`, two `app.pacing` answers. A cap is `null` (no cap) or a
number in 1..=10000; states the call does not name keep their cap. The call checks everything before it changes
anything. Errors:

- `pacing_unknown_state`: `state`, or a key of `caps`, is not `play`, `paused`, `menu` or `loading`.
- `pacing_bad_cap`: a cap is not `null` or a number in 1..=10000.
- `F0002`: neither `state` nor `caps`, or a parameter of the wrong type.

### Dynamic resolution outside play

`DynResController` steps the render scale up whenever the GPU time is under its band, so menu and pause frames
used to climb to native, and play stepped it down again. `DynResController::hold()` stops that: while held,
`update` returns the current permille and records nothing; `resume()` clears the rolling window, so the scale only
moves again after a full window of play frames. `regia_render::DynResGovernor` is the controller as a resource:
`feed(state, gpu_ms, frame_ms)` holds it whenever the state is not `play` (`hold_outside_play`, on by default) and
resumes it on the first play frame. The engine does not feed it yet (ADR-256's remaining step); the host does, once
per presented frame.

`render.dynres` answers `{ "present": false }` without a governor, else
`{ present, permille, held, hold_outside_play, bound, median_gpu_ms, budget_ms, min_permille, max_permille }`.

To check a capped menu on a desktop build:

```sh
regia agent call app.set_pacing_state '{"state":"menu","caps":{"menu":60}}'
regia agent call app.pacing '{}'          # effective_fps 60, measured_fps about 60
regia agent call app.set_pacing_state '{"state":"play"}'
```
