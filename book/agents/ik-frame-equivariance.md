# Two-bone IK in rotated parent frames

This is a CPU regression recipe for the existing `regia_animation::ik::solve_two_bone`
surface (spec 17 §§ 1–4, 6). Targets and poles are points in skeleton object space.
The chain must be contiguous, with unit scale. The fixture's object frame is also its
world frame; no entity placement or retargeting is involved.

The three fixed case tables make 292 solver calls. There is no randomized search,
file fixture, synchronization wait, or elapsed-time assertion.

Run the bounded integration target on a permitted build host:

```sh
cargo test -p regia_animation --test ik_frame_equivariance
cargo test -p regia_animation --test ik_edges --test twist_and_warp_edges
cargo test -p regia_animation
```

`reachable_triangles_follow_rotated_parent_frames` extends
`ik_edges::two_bone_reaches_fixed_seed_targets_and_keeps_bone_lengths`, which already
proves ordinary reach and length preservation under an identity parent. The new cases
use unequal lengths in both orders, nonidentity shoulder/elbow rotations, three limb
sizes, three off-axis target/pole pairs, weights 0 and 1, and three common rigid motions
of a nonidentity ancestor, target and pole. Every solve checks fixed segment lengths,
unchanged local translations/scales, and isolation of the parent and unrelated sibling.

`annulus_edges_match_the_reference_under_rigid_motion` brackets the inner and outer
physical reach boundaries and their numerical insets. For lengths `a`, `b`, the inset
is `min(0.0001, 0.25 * min(a, b))`; target distance clamps to
`[min(abs(a-b) + inset, a+b-inset), a+b-inset]`. An independent f64 construction uses
Heron's triangle area for the elbow height, chooses the projection sign for the
longer-lower-bone case, and projects the pole into the plane perpendicular to the
target direction. It calls no production bend helper. Existing
`ik::tests::a_chain_shorter_than_the_reach_margins_does_not_panic` covers the short
helper-bone guard; this recipe adds the transformed-frame differential oracle.

`quaternion_sign_equivalent_parents_produce_the_same_geometry` extends the twist
double-cover checks in `twist_and_warp_edges` to the two-bone parent's frame.
Comparisons use positions and rotated basis axes, so `q` and `-q` are equivalent.
No singular target or pole is used; the existing fallback cases stay in `ik_edges`.

Position tolerances scale with total limb length `L` and coordinate magnitude `M`:
`8e-5 * L + 8 * f32::EPSILON * M` for the reference elbow, twice that for the
comparison of two rounded solves, and `4e-6 * L + 8 * f32::EPSILON * M` for the
reference tip and segment lengths.
Near annulus edges, elbow height amplifies f32 distance roundoff. Rotated unit axes
use an absolute vector-distance bound of `3e-4`. Zero weight preserves positions and
axes exactly. The fixtures vary bone geometry, while every local scale stays one.

As a temporary negative control, replace `root_parent_global.inverse()` with
`root_parent_global` in the root's global-to-local assignment in `src/ik.rs`, run the
reachable target, and require a test-harness assertion failure (cargo exit 101),
not a build failure. Restore the assignment, verify that `ik.rs` has its original
SHA256, and require the entire target to pass. Never commit the mutation.

Existing agent coverage is recorded in [COVERAGE](../COVERAGE.md) § 2 under
`anim.list`, `anim.get_state` and `anim.set_param`, documented in
[Animation](animation.md). They inspect/drive an animator, not this pure solver's
elbow and tip oracle. This regression adds a verification recipe, no agent method
or runtime guide-topic registration. For interactive playback, retain that existing
animation recipe; screenshots do not replace these numeric assertions.
