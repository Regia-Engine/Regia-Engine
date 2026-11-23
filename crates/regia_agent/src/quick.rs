// SPDX-License-Identifier: MPL-2.0
//! The quick draw layer on the agent protocol (`book/agents/quick-draw.md`): `quick.draw_list`
//! reports what `regia_quick`'s `Draw` and `Draw3d` drew in the last frame, and the same summary
//! is the `QuickDraw` resource of `play.wait_until` / `play.expect` predicates.
//! `quick.draw2d_stats` reports the instanced 2D draw list (`regia_quick::draw2d`,
//! `book/agents/quick-draw2d.md`), also the `QuickDraw2d` predicate resource.

use regia_app::prelude::*;
use regia_quick::draw2d::Draw2dStats;
use regia_quick::{DrawFrozen, QuickDrawStats, QuickSettings};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// The last frame's quick-draw summary as JSON (also the `QuickDraw` predicate resource).
pub fn snapshot(world: &World) -> Result<Value, String> {
    let stats = world
        .get_resource::<QuickDrawStats>()
        .ok_or_else(|| "no QuickDrawStats in this app (add regia_quick::QuickPlugin)".to_string())?;
    let frozen = world.get_resource::<DrawFrozen>().is_some_and(|f| f.0);
    let samples = world.get_resource::<QuickSettings>().map_or(1, |s| s.samples);
    Ok(json!({
        "frame": stats.frame,
        "primitives": stats.primitives,
        "by_kind": stats.by_kind,
        "texts": stats.texts,
        "background": stats.background,
        "bounds": stats.bounds,
        "vertices": stats.vertices,
        "triangles": stats.triangles,
        "draw_calls": stats.draw_calls,
        "primitives_3d": stats.primitives_3d,
        "lines_3d": stats.lines_3d,
        "frozen": frozen,
        "samples": samples,
    }))
}

/// The last frame of the 2D draw list as JSON (also the `QuickDraw2d` predicate resource).
pub fn draw2d_snapshot(world: &World) -> Result<Value, String> {
    let stats = world
        .get_resource::<Draw2dStats>()
        .ok_or_else(|| "no Draw2dStats in this app (add regia_quick::draw2d::Draw2dPlugin)".to_string())?;
    let list = &stats.list;
    Ok(json!({
        "frame": stats.frame,
        "instances": list.instances,
        "by_kind": {
            "box": list.boxes,
            "outline": list.outlines,
            "segment": list.segments,
            "glyph": list.glyphs,
            "image": list.images,
        },
        "atlas_pages": list.atlas_pages,
        "glyphs_cached": list.glyphs_cached,
        "glyphs_rasterized": list.glyphs_rasterized,
        "glyphs_refused": list.glyphs_refused,
        "pages_staged": list.pages_staged,
        "bytes_uploaded": stats.gpu.bytes_uploaded,
        "instance_capacity": stats.gpu.instance_capacity,
        "layer_frames": stats.gpu.frames,
        "pipelines": stats.gpu.pipelines,
        "buffers": stats.gpu.buffers,
        "textures": stats.gpu.textures,
        "tiles": {
            "dirty_regions": world
                .get_resource::<regia_quick::draw2d::Draw2dSettings>()
                .is_some_and(|s| s.dirty_regions),
            "tiles": stats.gpu.tiles.tiles,
            "painted": stats.gpu.tiles.painted_tiles,
            "dirty": stats.gpu.tiles.dirty_tiles,
            "runs": stats.gpu.tiles.dirty_runs,
            "painted_runs": stats.gpu.tiles.painted_runs,
            "redrawn_instances": stats.gpu.tiles.redrawn_instances,
            "redrawn_total": stats.gpu.tiles.tiles_redrawn_total,
            "clean_frames": stats.gpu.tiles.clean_frames,
        },
    }))
}

/// `quick.draw2d_stats`: what the 2D draw list held and uploaded last frame.
pub fn draw2d_stats(world: &mut World) -> Result<Value, RpcError> {
    draw2d_snapshot(world).map_err(|message| {
        RpcError::not_found(
            message,
            "add regia_quick::draw2d::Draw2dPlugin after RenderPlugin",
        )
    })
}

/// `quick.draw_list`: what the quick layer drew last frame.
pub fn draw_list(world: &mut World) -> Result<Value, RpcError> {
    snapshot(world)
        .map_err(|message| RpcError::not_found(message, "add regia_quick::QuickPlugin after RenderPlugin"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_the_stats_resource() {
        let mut world = World::new();
        assert!(draw_list(&mut world).is_err());
        let mut stats = QuickDrawStats {
            frame: 3,
            primitives: 2,
            texts: vec!["score 7".into()],
            ..Default::default()
        };
        stats.by_kind.insert("rect".into(), 1);
        world.insert_resource(stats);
        let v = draw_list(&mut world).expect("stats present");
        assert_eq!(v["frame"], 3);
        assert_eq!(v["by_kind"]["rect"], 1);
        assert_eq!(v["texts"][0], "score 7");
        assert_eq!(v["frozen"], false);
    }
}
