# Motion-matching course pause and exact-step recipe

The `mm_locomotion` sample's `CourseLog.frames` counts host `Update` calls. It can increase
while an agent has paused gameplay. `CourseLog.simulation_frames` counts updates with a positive
gameplay delta, and `Course.elapsed`, matcher playback and the evaluated pose are simulation
state.

For a deterministic regression run, start the headless sample with the agent server and advance
to a checkpoint with a fixed number of updates. Read the course clock, hero transform, matcher
playback/search counters, evaluated pose and course log. Call `play.pause`, continue servicing
agent requests and host updates, then read the same state again. The host-update counter may
increase; the simulation-frame counter and all simulation state must remain unchanged. The
evaluated pose stays bit for bit frozen while paused, including across a running jump blend.

While paused, change the matcher filter with `anim.motion_match_set`. Its reported setting is
visible immediately, but its effect — the next search's best frame — is evaluated only after
`play.step` permits a simulation step; playback itself may hold the well-matching continuation
per the jump rule (spec 52 § 2.8). Compare that result with a fresh sample advanced through the
same course prefix and the same number of fixed steps (the comparison app needs the same plugin
set: entity indices, and the search cadence derived from them, depend on it). `play.step
{"frames": N}` runs exactly N simulation frames and pauses again. After `play.resume`, the next
update must continue from that exact state without catch-up. Bit-exact pose comparison across
two runs is only valid while no jump blend is running: the inertializer seeds a blend's decay
from the previous frame's output, so exact blend-decay bits depend on the pause pattern before
the jump (spec 52 § 7, `tests/blend_pause_pattern.rs`); time, distance, transform and matcher
state are always bit-exact.

At a course-leg boundary, pause just before the boundary and service several host updates. The
course must remain on its prior simulation time until a permitted step crosses the boundary.
This recipe uses the existing `play.pause`, `play.step`, `play.resume` and motion-matching agent
methods; it adds no protocol method.
