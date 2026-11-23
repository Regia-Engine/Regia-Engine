# Retargeting after a legal joint reorder

Related inspection method: `anim.retarget_check`. Its parameters, report and errors remain
specified in [Retargeting and crowds](retarget-crowds.md).

A skeleton stores parents before children (spec 17 § 1). Sibling subtrees may be interleaved
while keeping that rule. Reindex parents, local binds, inverse binds, clip joint ids and the
root-motion joint together; explicit role maps continue to name the same stable joints.
Validate both skeletons before building the retargeter. Compare object-space transforms by
joint name and root-motion translation and rotation at keys and intermediate samples. Rotation
matrices avoid treating equivalent quaternion signs as a difference.

The CPU regression is:

```sh
cargo test -p regia_animation --test retarget_joint_reindex
```

It uses three legal storage orders, rolled bind axes, different limb lengths, nonzero root
translation and one animated source helper that remains unmapped. An independent ancestry-chain
matrix check verifies the fixture geometry and inverse binds before retarget output is compared.
The absolute matrix-entry tolerance is `3e-5`: enough for accumulated `f32` roundoff in these
unit-scale rigs of depth six, while resolving changes much smaller than a millimeter.

The cases with changed shoulder rest directions exercise a branch with two equally near mapped
descendants. Their result is recorded in the lane release fragment and report. This regression
uses the existing API and protocol surface; it adds no method, asset format or CLI command.

The changed-shoulder-rest cases found a defect: a legal target reorder changed the chest
transform when two mapped children were equally near, because the nearer-descendant pick fell to
target storage order. The retargeter now breaks that tie by joint name (integration
integ-v049-recover-unsure-c, ADR-5863), and all seven cases pass.
