# First-import fixture (ADR-7788)

A tiny project as a fresh clone has it: sources and their `.meta` sidecars, no `.regia/` (no asset database,
no cooked cache). `asset_package::first_import_tests` copies it to a temp folder, cooks it the way the editor
does on open, and checks that the first cook reports no missing references. The material names its color map by
path and its normal map by asset id, so the texture ids must resolve before or while the material cooks.
