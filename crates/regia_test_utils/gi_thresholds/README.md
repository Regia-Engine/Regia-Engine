# GI validation records

One file per standard GI scene (`<scene>.txt`) holding the recorded error of each GI technique against the path
tracer: `scene technique rel_rmse shadow_bias energy_ratio`. `regia_pt/tests/gi_validation.rs` fails when a
technique's error grows past its record (ADR-423, `book/agents/gi-validation.md`).

Write them with `REGIA_GI_RECORD=1 cargo test -p regia_pt --test gi_validation` on the lavapipe reference
adapter only, and only for an intended change; say why in the commit message, as for a re-blessed golden.
