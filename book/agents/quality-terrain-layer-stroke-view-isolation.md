# Painted terrain layer stroke-view regression

`LayerWeightMap::layer_mut` creates a temporary, internal `StrokeTarget<u8>` view for one
painted layer. It has no direct agent-protocol method. `terrain.stroke` paints through this target
when called with `mode: "paint"` and a `layer`; `terrain.save_paint` and `terrain.load_paint`
serialize and restore the map.

The focused regression is `cargo test -p regia_terrain --test layer_stroke_view_isolation`. It uses
a 5×3 map with three layers and a separate flat byte-array oracle. The deterministic sequence
checks reads and writes through reacquired views, negative and out-of-grid positions, map corners
and edges, alternating edits to two layers, invalid layer requests, a serialized reload, reverse
replay of the write log, plane equality, blank state and exact serialized restoration. A
temporary wrong row stride makes the oracle fail, demonstrating that the expected layout is
independent of the implementation.

The agent-facing paint flow is covered by the existing terrain stroke handler tests; the view
itself remains an internal primitive. Existing lowercase paint-file error codes are
`height_io_write`, `height_io_read`, `height_io_decode` and `height_io_size_mismatch` for
`terrain.save_paint` / `terrain.load_paint`. Invalid paint stroke parameters use the existing
`F0002` invalid-parameters response.
