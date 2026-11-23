# Trail expiry and path-distance regression

`TrailRig` is an internal VFX primitive without an agent-protocol method. This recipe checks it
through its existing public API and adds no agent method or error code.

Run `cargo test -p regia_vfx --test trail_expiry_distance_contract`. The deterministic sequence
compares `total_distance()` with an independently summed polyline while samples are decimated,
age out, and motion resumes after the sample list empties. It also checks the exact lifetime
boundary, equivalent stationary aging partitions, and strip rebuilding with two camera-right
vectors. The strip checks finite bounded positions, six vertices per segment, triangle-list
alignment, camera-dependent positions, and alpha in range.
