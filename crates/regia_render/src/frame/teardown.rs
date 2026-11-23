// SPDX-License-Identifier: MPL-2.0
//! Timed teardown of a [`FrameRenderer`]: the clean exit drops the runtime renderer part by
//! part and logs the slowest parts (ADR-188 step 5), so a slow exit in a tester's log names
//! what was slow.

use std::time::{Duration, Instant};

use super::FrameRenderer;

/// How long dropping a [`FrameRenderer`] took, by part ([`FrameRenderer::drop_timed`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RendererDropReport {
    /// The whole drop.
    pub total: Duration,
    /// Every timed part, in drop order; `"rest"` is everything not named.
    pub parts: Vec<(&'static str, Duration)>,
}

impl RendererDropReport {
    /// The `n` slowest parts, slowest first.
    #[must_use]
    pub fn slowest(&self, n: usize) -> Vec<(&'static str, Duration)> {
        let mut parts = self.parts.clone();
        parts.sort_by_key(|p| std::cmp::Reverse(p.1));
        parts.truncate(n);
        parts
    }

    /// `"meshes 812 ms, shared textures 400 ms"`: the `n` slowest parts for a log line.
    #[must_use]
    pub fn slowest_text(&self, n: usize) -> String {
        self.slowest(n)
            .iter()
            .map(|(name, d)| format!("{name} {:.1} ms", d.as_secs_f64() * 1e3))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl FrameRenderer {
    /// Stops the renderer's background workers (material-graph builds) without waiting: the
    /// clean exit's first step, so a build in flight ends while the GPU drains and the
    /// renderer's drop does not wait for it. Graph materials that are not built yet keep their
    /// fallback.
    pub(crate) fn stop_background_workers(&mut self) {
        self.graph_cache.stop_workers();
    }

    /// Drops this renderer part by part and reports how long each part took. Frees exactly
    /// what dropping it does; call it once the GPU is idle (the clean exit does, on a helper
    /// thread).
    #[must_use]
    pub fn drop_timed(self) -> RendererDropReport {
        let start = Instant::now();
        let mut parts = Vec::with_capacity(48);
        let mut mark = start;
        let mut part = |name: &'static str, parts: &mut Vec<(&'static str, Duration)>| {
            let now = Instant::now();
            parts.push((name, now - mark));
            mark = now;
        };
        {
            let this = self;
            let Self {
                meshes,
                materials,
                shared_textures,
                graph_cache,
                graph_layouts,
                objects,
                targets,
                post,
                lens,
                texture_targets,
                outdoor,
                culler,
                visbuffer,
                ssao_pass,
                contact_pass,
                sun_trace,
                taau_pass,
                tsr_pass,
                fsr3_pass,
                vendor_upscaler,
                sun_pages,
                atlas_tiles,
                emissive_cache,
                ddgi,
                highend,
                shared_draw,
                gizmo_renderer,
                scalar_pass,
                belt,
                stream_frame,
                finish_workers,
                glass_gpu,
                translucent_maps,
                texture_views,
                texture_view_handles,
                pipes,
                decal_receiver,
                translucent,
                view_bgs,
                cluster_bg,
                gi_field,
                probe_atlas,
                transients,
                keyed_transients,
                csm,
                atlas,
                ibl,
                gpu_timer,
                ..
            } = this;
            // Material-graph workers first: their join is bounded, and a worker in a compile
            // holds a device clone.
            drop(graph_cache);
            part("material graph workers", &mut parts);
            drop(finish_workers);
            part("encode workers", &mut parts);
            drop(shared_draw);
            part("shared pass draw (terrain)", &mut parts);
            drop(objects);
            part("objects", &mut parts);
            drop(meshes);
            part("meshes", &mut parts);
            drop(materials);
            part("materials", &mut parts);
            drop(shared_textures);
            part("shared textures", &mut parts);
            drop((texture_views, texture_view_handles));
            part("texture views", &mut parts);
            drop((targets, texture_targets, transients, keyed_transients));
            part("render targets", &mut parts);
            drop((post, lens));
            part("post chain", &mut parts);
            drop(outdoor);
            part("sky and atmosphere", &mut parts);
            drop((culler, visbuffer));
            part("culler and visibility buffer", &mut parts);
            drop((ssao_pass, contact_pass, sun_trace));
            part("ssao and contact shadows", &mut parts);
            drop((taau_pass, tsr_pass, fsr3_pass, vendor_upscaler));
            part("temporal upscalers", &mut parts);
            drop((sun_pages, atlas_tiles, csm, atlas, translucent_maps));
            part("shadow maps", &mut parts);
            drop((emissive_cache, ddgi, gi_field, probe_atlas, ibl));
            part("gi and probes", &mut parts);
            drop(highend);
            part("high-end passes", &mut parts);
            drop((
                gizmo_renderer,
                scalar_pass,
                glass_gpu,
                decal_receiver,
                translucent,
            ));
            part("gizmos, glass, decals, translucent layers", &mut parts);
            drop((belt, stream_frame, gpu_timer));
            part("staging and timers", &mut parts);
            drop((view_bgs, cluster_bg, graph_layouts));
            part("bind groups", &mut parts);
            drop(pipes);
            part("pipelines", &mut parts);
            // Everything not named drops at the end of this block.
        }
        part("rest", &mut parts);
        RendererDropReport {
            total: start.elapsed(),
            parts,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slowest_sorts_and_formats() {
        let report = RendererDropReport {
            total: Duration::from_millis(30),
            parts: vec![
                ("a", Duration::from_millis(5)),
                ("b", Duration::from_millis(20)),
                ("c", Duration::from_millis(1)),
            ],
        };
        assert_eq!(
            report.slowest(2).iter().map(|p| p.0).collect::<Vec<_>>(),
            ["b", "a"]
        );
        assert_eq!(report.slowest_text(1), "b 20.0 ms");
    }

    #[test]
    fn drop_timed_on_a_fresh_renderer_reports_every_part() {
        let report = FrameRenderer::new().drop_timed();
        assert_eq!(report.parts.last().map(|p| p.0), Some("rest"));
        assert!(report.parts.len() > 20);
        let sum: Duration = report.parts.iter().map(|p| p.1).sum();
        assert!(sum <= report.total + Duration::from_millis(1));
    }
}
