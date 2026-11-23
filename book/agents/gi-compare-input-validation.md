# GI comparison input validation

The existing [`cargo xtask gi-compare`](gi-compare.md) command checks image shapes before
indirect isolation, sky replacement, comparison and leak scoring. Each consumed plane must have
exactly `width * height` row-major elements, and each source image must match the requested
width and height separately. A 2 × 3 image cannot substitute for a 3 × 2 image.

The checks cover every baked reconstruction, DDGI and direct RGB capture; reference beauty,
depth and AOV planes; and the optional reference trace without emitters. The four operands of
reference indirect isolation are `indirect_diffuse`, `indirect_specular`, reference `direct`
and the optional trace's `direct`. The leak boundary also checks the mask length. An empty
adaptive sample map remains valid for a uniform trace. A mask selecting no pixels and a black
reference with no leak denominator remain valid and retain their existing absent/null results.

There is no cropping policy. The existing `--size WIDTHxHEIGHT` option chooses the size before
rendering both sources. It does not resize a returned plane. The sheet's display thumbnails are
scaled only after comparison. The pure functions in `gi_compare::metrics` keep their signatures;
callers outside the command must supply aligned planes themselves.

A refused shape follows the existing command error path, exiting with code 2 and printing
`gi-compare: [image_shape] ...`. The message identifies the source/plane and the expected and
actual dimensions or element counts. An HDR capture whose dimensions differ from the requested
frame retains the existing `render_failed` diagnostic. Existing codes such as `views_size`
continue to reject zero or out-of-range requested dimensions before rendering.

The command generates images from `--scene` and `--views`; it does not accept RGB, EXR, depth or
mask files as comparison inputs. Malformed returned planes are therefore exercised at its CPU
boundary, without a new image-loading flag or report schema.

Run the boundary regressions without an adapter:

```sh
cargo test -p xtask gi_compare
```

With a trusted running agent attached to this repository, the existing test job methods can
select the same library tests. The host must grant `build` permission and use the build wrapper
required by its machine; these calls do not grant permissions or choose a GPU:

```sh
regia agent call agent.run_tests '{"package":"xtask","target":"lib","filter":"gi_compare","dry_run":true}'
regia agent call agent.run_tests '{"package":"xtask","target":"lib","filter":"gi_compare"}'
```

Inspect the returned job id with `agent.job_status`, passing its `next` cursor as `since` on
subsequent calls. Require `finished: true`, `state: "done"` and `result.ok: true`; inspect
`result.failures` and `result.errors` on failure. See [Jobs and test runs](jobs.md).

```sh
regia agent call agent.job_status '{"job":"tests:1","since":0}'
```

The test fixtures use generated tiny planes and temporary command files. No reference images,
quality thresholds, renderer code or goldens change. Valid scores, leak values and summary
serialization retain their existing behavior.
