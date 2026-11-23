// SPDX-License-Identifier: MPL-2.0
//! Leak control of the baked GI lookup on the agent protocol (ADR-1105,
//! `book/agents/baked-gi-visibility.md`).
//!
//! `render.get_baked_gi_visibility` reads how the lookup weighs a corner probe that its distance
//! moments find partly hidden: the visibility filter (`per_tap`, the default, or `moments`, DDGI
//! as published) and the light-bleeding reduction. `render.set_baked_gi_visibility` changes them
//! on the renderer (for a comparison: `preset: "legacy"` is the lookup before ADR-1105).
//! `render.sample_baked_gi` (`gi_mode.rs`) takes `visibility_filter` and `leak_reduction` as
//! per-call overrides, so an agent can check a suspected leak (a bright patch on a dark floor
//! beside a wall) against the legacy lookup without a screenshot.

use regia_app::prelude::*;
use regia_render::baked_gi::{GiVisibilityFilter, MAX_LEAK_REDUCTION};
use regia_render::{BakedGiSettings, RuntimeRenderer};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn missing() -> RpcError {
    RpcError::not_found(
        "no RuntimeRenderer resource: this app has no runtime renderer",
        "add regia_render::RenderPlugin",
    )
}

fn visibility_json(s: &BakedGiSettings) -> Value {
    json!({
        "visibility_filter": s.visibility_filter.name(),
        "leak_reduction": s.leak_reduction,
        "effective_leak_reduction": s.effective_leak_reduction(),
    })
}

/// `render.get_baked_gi_visibility`: the visibility filter and light-bleeding reduction in use,
/// the defaults, the reduction's cap and whether a volume is installed.
pub(crate) fn get_visibility(world: &mut World) -> Result<Value, RpcError> {
    let r = world.get_resource::<RuntimeRenderer>().ok_or_else(missing)?;
    let s = r.0.baked_gi_settings();
    let mut out = visibility_json(&s);
    if let Some(map) = out.as_object_mut() {
        map.insert("default".into(), visibility_json(&BakedGiSettings::default()));
        map.insert("max_leak_reduction".into(), json!(MAX_LEAK_REDUCTION));
        map.insert(
            "filters".into(),
            json!([
                GiVisibilityFilter::PerTap.name(),
                GiVisibilityFilter::Moments.name()
            ]),
        );
        map.insert("volume_installed".into(), json!(r.0.baked_gi().is_some()));
    }
    Ok(out)
}

/// `render.set_baked_gi_visibility`: `preset` (`"default"` or `"legacy"`) first, then
/// `visibility_filter` (`"per_tap"` or `"moments"`) and `leak_reduction` (0..=0.95) on top. At
/// least one is required. Returns the settings before and after.
pub(crate) fn set_visibility(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let preset = match params.get("preset") {
        None => None,
        Some(v) => match v.as_str() {
            Some("default") => Some(BakedGiSettings::default()),
            Some("legacy") => Some(BakedGiSettings::legacy_visibility()),
            _ => {
                return Err(RpcError::invalid_params(
                    "`preset` must be \"default\" (per_tap, leak_reduction 0.25) or \"legacy\" \
                     (moments, 0: the lookup before ADR-1105)",
                ));
            }
        },
    };
    let filter = match params.get("visibility_filter") {
        None => None,
        Some(v) => Some(
            v.as_str()
                .and_then(GiVisibilityFilter::from_name)
                .ok_or_else(|| {
                    RpcError::invalid_params("`visibility_filter` must be \"per_tap\" or \"moments\"")
                })?,
        ),
    };
    let amount = match params.get("leak_reduction") {
        None => None,
        Some(v) => {
            let k = v
                .as_f64()
                .map(|k| k as f32)
                .filter(|k| (0.0..=MAX_LEAK_REDUCTION).contains(k));
            Some(k.ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "`leak_reduction` must be a number in 0..={MAX_LEAK_REDUCTION} (0 = off)"
                ))
            })?)
        }
    };
    if preset.is_none() && filter.is_none() && amount.is_none() {
        return Err(RpcError::invalid_params(
            "give `preset` (\"default\" or \"legacy\"), `visibility_filter` (\"per_tap\" or \
             \"moments\") or `leak_reduction` (0..=0.95)",
        ));
    }
    let mut r = world.get_resource_mut::<RuntimeRenderer>().ok_or_else(missing)?;
    let before = r.0.baked_gi_settings();
    let mut after = before;
    if let Some(p) = preset {
        after.visibility_filter = p.visibility_filter;
        after.leak_reduction = p.leak_reduction;
    }
    if let Some(f) = filter {
        after.visibility_filter = f;
    }
    if let Some(k) = amount {
        after.leak_reduction = k;
    }
    r.0.set_baked_gi_settings(after);
    Ok(json!({
        "before": visibility_json(&before),
        "after": visibility_json(&after),
        "note": "applies from the next frame; the host's own FrameRenderer::set_baked_gi_settings \
                 calls replace it, and a level's baked GI restores the settings it found when the \
                 level unloads",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_sets_the_visibility_settings() {
        let mut world = World::new();
        assert!(get_visibility(&mut world).is_err(), "no renderer");
        world.insert_resource(RuntimeRenderer::default());
        let got = get_visibility(&mut world).expect("get");
        assert_eq!(got["visibility_filter"], "per_tap");
        assert_eq!(got["default"]["visibility_filter"], "per_tap");
        assert!((got["leak_reduction"].as_f64().unwrap_or(-1.0) - 0.25).abs() < 1e-6);
        assert!((got["max_leak_reduction"].as_f64().unwrap_or(-1.0) - 0.95).abs() < 1e-6);
        assert_eq!(got["volume_installed"], false);

        let set = set_visibility(&mut world, &json!({ "preset": "legacy" })).expect("legacy");
        assert_eq!(set["before"]["visibility_filter"], "per_tap");
        assert_eq!(set["after"]["visibility_filter"], "moments");
        assert_eq!(set["after"]["leak_reduction"], 0.0);
        assert_eq!(
            world.resource::<RuntimeRenderer>().0.baked_gi_settings(),
            BakedGiSettings::legacy_visibility()
        );
        // Fields on top of a preset; the other settings are untouched.
        set_visibility(
            &mut world,
            &json!({ "preset": "default", "visibility_filter": "moments", "leak_reduction": 0.5 }),
        )
        .expect("set");
        let s = world.resource::<RuntimeRenderer>().0.baked_gi_settings();
        assert_eq!(s.visibility_filter, GiVisibilityFilter::Moments);
        assert_eq!(s.leak_reduction, 0.5);
        assert_eq!(s.normal_bias, BakedGiSettings::default().normal_bias);

        for bad in [
            json!({}),
            json!({ "preset": "dark" }),
            json!({ "visibility_filter": "pcf" }),
            json!({ "leak_reduction": 1.0 }),
            json!({ "leak_reduction": -0.1 }),
            json!({ "leak_reduction": "high" }),
        ] {
            let err = set_visibility(&mut world, &bad).expect_err("rejected");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{bad}");
        }
        // A rejected call changed nothing.
        assert_eq!(world.resource::<RuntimeRenderer>().0.baked_gi_settings(), s);
    }

    #[test]
    fn leak_reduction_boundaries_and_null_fields() {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer::default());
        let at = |world: &mut World, k: Value| set_visibility(world, &json!({ "leak_reduction": k }));
        assert!(at(&mut world, json!(0.0)).is_ok(), "0 turns it off");
        assert_eq!(
            world
                .resource::<RuntimeRenderer>()
                .0
                .baked_gi_settings()
                .leak_reduction,
            0.0
        );
        assert!(
            at(&mut world, json!(0.95)).is_ok(),
            "the maximum itself is allowed"
        );
        assert_eq!(
            world
                .resource::<RuntimeRenderer>()
                .0
                .baked_gi_settings()
                .leak_reduction,
            MAX_LEAK_REDUCTION
        );
        let kept = world.resource::<RuntimeRenderer>().0.baked_gi_settings();
        for bad in [
            json!(0.9501),
            json!(1.0e300),
            json!(-1.0e300),
            json!(null),
            json!(true),
        ] {
            assert!(at(&mut world, bad.clone()).is_err(), "{bad}");
            assert_eq!(
                world.resource::<RuntimeRenderer>().0.baked_gi_settings(),
                kept,
                "{bad}"
            );
        }
        for bad in [
            json!({ "preset": null }),
            json!({ "preset": 1 }),
            json!({ "visibility_filter": null }),
            json!({ "visibility_filter": 3 }),
        ] {
            assert!(set_visibility(&mut world, &bad).is_err(), "{bad}");
        }
        // The same call twice reports no change the second time.
        let a = set_visibility(&mut world, &json!({ "visibility_filter": "moments" })).expect("a");
        let b = set_visibility(&mut world, &json!({ "visibility_filter": "moments" })).expect("b");
        assert_eq!(a["after"], b["after"]);
        assert_eq!(b["before"], b["after"]);
        // Without a renderer, valid params are not-found and bad params are invalid.
        let mut bare = World::new();
        assert_eq!(
            set_visibility(&mut bare, &json!({ "leak_reduction": 0.1 }))
                .expect_err("bare")
                .code,
            crate::protocol::NOT_FOUND
        );
        assert_eq!(
            set_visibility(&mut bare, &json!({})).expect_err("bare").code,
            crate::protocol::INVALID_PARAMS
        );
    }
}
