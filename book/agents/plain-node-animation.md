# Plain-node animation

Plain-node glTF translation, rotation and scale channels import as
`GltfAnimationImport::node_clips` and cook to `#nodeclip/<animation>` sub-assets.
They work without skins and can coexist with morph tracks. Each clip's optional
`node_animation` retains source indices, names, parent slots, bind defaults and
exact STEP/LINEAR/CUBICSPLINE channels. Both cooked writers retain them losslessly.

Use the existing `anim.clip_report { entity, compression: false }` on an
`Animator::single(clip, false)` to inspect a clip. Each clip report adds
`node_animation` (null for old/skeletal-only clips), containing `version`,
`nodes`, `validation_error` and `samples`. Samples at zero, half duration and
full duration list source `index`, `is_joint`, `local`, `world` and `delta`.
Matrices are 16 column-major numbers. A singular bind gives `delta: null`.
Successful samples have `error: null`. `validation_error` and sample `error`
are diagnostic text, not new RPC error codes. Existing `not_found` and
`invalid_params` behavior remains unchanged.

`anim.list` / `anim.get_state` also expose `clip.node_tracks`, the count of
nodes with authored TRS channels. Parent slots refer to the report's `nodes`
array; source indices are preserved even when nodes are reordered topologically.
Duplicate names never alias two parts.

A host checks `clip.node_animation` and calls `NodeAnimation::sample(time, &mut out)`. Bind results by source index, separately per model
instance. For vertices baked into the imported scene frame, apply `delta`.
For geometry still in source-local space, use `world`. Keep `is_joint` nodes
on the skin path. Do not apply the import axis/scale twice. This API does not
write ECS transforms or blend node tracks through an animation graph; the host
owns its rigid-part presentation and playback clock.

Verification: import a procedural node moving two meters along X; after the
default forward conversion the midpoint report has world translation X = -1
and the final sample X = -2. The source has no skin. Repeat after cooking with
both writers and after slicing; compare matrices by source index.
