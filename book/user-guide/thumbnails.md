# Batch thumbnails

`ThumbnailRenderer` renders many small pictures of models or entities through one device and one
warm frame renderer. Shaders and pipelines compile for the first job only, and jobs of equal size
share their render targets. `capture_pane` renders one full pane at a time; this is for grids of
cards.

<!-- ignore-reason: thumbnail rendering requires a GPU device and editor render target -->
```rust,ignore
use regia_editor::thumbnails::{ThumbJob, ThumbnailRenderer};
use regia_editor::views::ViewKind;
use regia_render::DebugViewMode;

let mut thumbs = ThumbnailRenderer::new(&device);
let images: Vec<egui::ColorImage> = thumbs.render(&[
    ThumbJob::model(model.clone(), 128),
    ThumbJob::model(model.clone(), 128).viewed(ViewKind::Side),
    ThumbJob::model(model, 128).viewed((2.4, 0.3)).debug(DebugViewMode::Wireframe),
]);
// Entities of a world: thumbs.render_in(&mut world, &[ThumbJob::entity(entity, 96)]);
```

- A `ThumbJob` has a subject (`Arc<LoadedModel>` or an `Entity`), a `view` (`ViewKind` or
  `(yaw, pitch)` in radians), a `size` (square, 16 to 2048 pixels) and a `debug` mode (`Lit` and
  `Wireframe`; the other modes draw lit).
- The stage is the asset viewer's: centered, framed by the subject's bounds, a key light, the
  backdrop sky and the editor quality profile. Views are perspective. A picture depends only on its
  job, so identical jobs give identical pixels.
- `render` returns one `ColorImage` per job, in order. A job that cannot draw (no geometry, an
  entity job given to `render` instead of `render_in`, a GPU failure) is a transparent picture of
  its size, counted in `stats().failed`.
- `stats()` reports the last call, with the frame-path counters: after a warm-up, `pipelines` is 0.
- Agents: `editor.thumbnails` ([agent guide](../agents/thumbnails.md)).
