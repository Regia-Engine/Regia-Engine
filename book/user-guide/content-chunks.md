# Shared glTF images and content chunks

Starting with v0.5.1, shared external textures live in the manifest at their **image path**, such
as `assets/shared.png`, or at `assets/shared.png#gltf_texture/<settings hash>` for a settings
variant. They no longer inherit chunk selectors through `model.gltf#texture/N`. Chunk globs
are relative to `assets/`; selectors for the image path apply to its variants as well.

For example, `assets/models/a.gltf` and `assets/models/b.gltf` both use `assets/shared.png`:

```toml
[cook.chunks.a]
include = ["models/a.gltf"]
delivery = "on_demand"

[cook.chunks.b]
include = ["models/b.gltf"]
delivery = "on_demand"
```

Both chunks follow their material dependencies with equal default priority. No selector matches
the image itself, so this tie puts the shared texture in `base`: `SharedPolicy::Base`, the default
`[cook.chunk_options] shared = "base"`. Base downloads and Android install-time content grow by
that texture's size even though the models are on demand. This changes placement for existing
projects whose private glTF textures previously stayed with their models.

To keep a copy with each claiming chunk, add `[cook.chunk_options]` with `shared = "duplicate"`.
The copies have the same asset id. This avoids base growth at the cost of repeated pak bytes.
The tie rule applies to equal highest-priority **follow claims**. Direct selectors of equal
priority that match the same image still fail with `chunk_rule_conflict`.

An explicit image selector takes precedence over follow claims. For the example above, adding
`"shared.png"` to chunk `a`'s `include` places the texture in `a`. Chunk `b` must then declare
`requires = ["a"]`; otherwise cooking fails with `chunk_unlisted_reference`. The same applies
when the image is placed in a separate textures chunk. A base asset cannot reference an image
in an optional chunk (`chunk_base_reference`); place that dependency in base instead.

After upgrading, recook and repack every platform, inspect `regia assets chunks` and
`regia assets why assets/shared.png`, and update image selectors and `requires` as needed.
Agents can inspect the same placement with `content.why` and the registry methods. The
[rules and delivery guide](../agents/content-chunks.md) covers priorities, pins and delivery.
