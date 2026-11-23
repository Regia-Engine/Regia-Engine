# CPU VFX emission counts across step partitions

Use the existing [VFX methods](vfx.md#systems-list-describe-spawn-change): `vfx.spawn`,
`vfx.describe`, `vfx.systems` and `vfx.stats`. `vfx.systems` reports CPU scheduled `live`
particles, including jobs awaiting upload. A drain uploads jobs and retains their live budget.
No extra protocol method is needed for the CPU scheduling checks below.

`crates/regia_vfx/tests/emission_count_partitions.rs` checks the public `VfxInstance`
scheduler directly. With fixed LOD, count scale and intensity, nonbinding budgets and particle
lifetimes longer than the episode, the supported bounded cases have totals:

- Stationary rate: `floor(rate * total_elapsed * count_scale * lod_scale * intensity)`.
- Straight travel: `floor(total_distance * count_scale * lod_scale * intensity / every)`.

The test inputs use exact binary fractions: nonnegative finite intervals, zero-time pauses,
positive distance spacing, an axis segment and a 3-4-5 straight segment with analytic lengths.
Zero-time pauses keep the origin fixed; moving the origin during a zero-time step is outside
the invariant model.
Every step drains jobs, checks all attributes are finite and checks the total against the formula.
The suite does not assert identical spawn times, moving origins or GPU trajectories across
partitions. It does not promise arbitrary floating-point inputs are partition invariant.
A burst fires once on its first eligible positive-time schedule; zero-time schedules and
consecutive drains cannot replay it.

Timing windows use the instance age **after** the step. A positive-time step is eligible when
`age >= delay` and, for a finite duration, `age - delay <= duration`. An eligible step charges
its whole elapsed time or traveled distance; the scheduler does not clip the step at a timing
boundary. A step that crosses the entire window and ends after it schedules nothing. A zero-duration
window can be hit exactly or skipped. A zero-time schedule does not activate a burst or age an
instance. Format v6 has no loop setting: `duration: None` keeps rate/distance emitters active until
stopped, and a spent finite window or burst does not automatically start another cycle. The suite
checks explicit endpoint/count tables for these boundaries rather than continuous integration.

Per-call global/smoke headroom can make totals depend on the partition. Separate cases check each
step stays within those caps and the system's live budget, and that drains do not free live budget.
This coverage has no golden, rendering, hardware performance or physical-device acceptance claim.
