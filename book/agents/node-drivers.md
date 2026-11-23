# Rigid node drivers

`anim.node_drivers` reports procedural hinges, spins and slews on plain nodes
(ADR-8410). The call is read-only.

Omit `entity` to list every entity that has a `NodeDriver`. The list result is
`entities`, an array of the same object the single-entity call returns.

Pass `entity`, the integer id from `scene.query_entities`, to read one entity.
The result is `{ entity, drives, params }`. Each drive has `node`, `kind`
(`hinge`, `spin` or `slew`), `input`, `primary`, `secondary` and `value`.
A hinge adds `axis`, `min`, `max` and `rate`. A spin adds `axis`. A slew adds
`yaw_axis`, `pitch_axis`, `max_rate`, `limits` and `pitch_value`. Angles are
degrees. `params` is the stored list in order; the first copy of a name is the
one the drives read. A non-finite number is JSON `null`.

`F0002` (`invalid_params`): `entity` is present and is not an integer id.
`F0003` (`not_found`): that entity does not exist, or it has no `NodeDriver`.

The game loads its own sidecar. There is no new cooked format. Insert
`PlainNodePlayer`, `PlainNodePose`, `NodeDriver`, `NodeDriveParams` and
`NodeDriveState` on the entity. `AnimationPlugin` samples the player's clip in
`PostUpdate`, after skeletal apply, then steps the drives. The driven matrix is
the clip-local matrix times each rotation on the right, in list order. A joint
keeps the clip matrix. A missing parameter reads as 0. A slew's pitch parameter
is `{input}.pitch`.

Hinge: the input is the target in degrees, clamped to the closed limits and
approached at `rate` degrees per second. An angle already outside the limits
steps inward; it is not snapped. Spin: the input is degrees per second about
the axis, wrapped into `[0, 360)`. Slew: yaw and pitch each move by at most
`max_rate` degrees per second. A zero or negative rate does not move. This is
presentation math. It is not a lockstep checksum.
