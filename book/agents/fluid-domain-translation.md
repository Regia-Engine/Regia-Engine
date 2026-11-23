# Fluid domain-origin translation regression

This regression guards the CPU solver's domain-local coordinate behavior. It uses only the existing `regia_fluid`
surface; the feature and methods under test are listed in [Particle liquid](fluid.md) in `book/COVERAGE.md`.

Run `cargo test -p regia_fluid --test domain_translation` on the CPU path. The fixed-seed fixtures pair identical grids
after moving the origin and all world-space inputs by integer multiples of the binary-exact cell size, including negative
components. Every fixed step compares translated-back positions, velocities, particle cell membership, the actual
projected liquid mask, each pressure cell, liquid volume, moving-solid poses and (for the moving-solid fixture) linear
and angular impulses. Position, velocity, pressure and volume tolerances scale from cell size, timestep and density.
The test intentionally does not compare state digests,
which hash world-space particle coordinates.

The fixtures cover a passive particle cloud, a shaken bounded tank with a static box and moving box, and a continuous source with
a sink. They need no files, GPU, agent method or elapsed-time synchronization.
