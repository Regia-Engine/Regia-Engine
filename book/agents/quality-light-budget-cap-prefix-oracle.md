# Visible-light budget ranking regression

`light_budget::select_visible_lights` is an internal CPU primitive; it has no agent-protocol
method. The renderer's local-light and frame inspection surfaces remain unchanged, so there is
no new agent call or error code to exercise for this regression.

The unit test `light_budget::tests::cap_sweep_matches_independent_importance_oracle_and_drop_counts`
uses a fixed +X view and synthetic point lights. It computes the documented ranking independently
from candela times projected-radius squared, breaks exact ties by original list index, checks each
cap from zero through the authored count, and verifies authored order returns when the full set
fits. It also checks nested ranking prefixes only while both compared caps drop lights.

`ranking_is_translation_invariant_and_shadow_flags_do_not_change_fitting_order` turns shadows on
for a fitting set and translates the camera and lights by the same vector, checking identities at
both an over-cap and a fitting cap. `hard_upload_cap_boundary_is_clamped_without_losing_best_ranked_identities`
checks the 512-light boundary and the 513th authored light with the requested cap above the storage
limit. These are CPU-only checks; they do not claim a GPU or platform pass.

Run them with:

```sh
cargo test -p regia_render --lib light_budget::tests::cap_sweep_matches_independent_importance_oracle_and_drop_counts
cargo test -p regia_render --lib light_budget::tests::ranking_is_translation_invariant_and_shadow_flags_do_not_change_fitting_order
cargo test -p regia_render --lib light_budget::tests::hard_upload_cap_boundary_is_clamped_without_losing_best_ranked_identities
```
