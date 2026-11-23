// SPDX-License-Identifier: MPL-2.0
//! The host uses the exact curve evaluators used by patch playback.

use regia_audio::{
    LayerOutputs, PatchCurveDef, PatchLayerDef, PatchParamDef, SmoothedParam, layer_outputs, polyline,
};
use std::collections::HashMap;

#[test]
fn public_polyline_clamps_and_interpolates() {
    assert_eq!(polyline(&[], 1.0), 0.0);
    assert_eq!(polyline(&[(1.0, 3.0)], -1.0), 3.0);
    let points = [(0.0, -6.0), (1.0, 0.0), (2.0, 12.0)];
    assert_eq!(polyline(&points, -1.0), -6.0);
    assert_eq!(polyline(&points, 0.5), -3.0);
    assert_eq!(polyline(&points, 3.0), 12.0);
}

#[test]
fn public_layer_outputs_composes_the_playback_law() {
    let curve = |points| PatchCurveDef {
        parameter: "drive".into(),
        points,
    };
    let layer = PatchLayerDef {
        name: "loop".into(),
        sound: Some("tone".into()),
        keys: vec![],
        parameter: None,
        volume_db: -3.0,
        volume_curves: vec![curve(vec![(0.0, -12.0), (1.0, 0.0)])],
        pitch_curves: vec![curve(vec![(0.0, 0.5), (1.0, 1.5)])],
        filter_curves: vec![
            curve(vec![(0.0, 200.0), (1.0, 1_000.0)]),
            curve(vec![(0.0, 400.0)]),
        ],
        one_shot: None,
        granular: None,
    };
    let params = HashMap::from([(
        "drive".into(),
        SmoothedParam {
            def: PatchParamDef {
                name: "drive".into(),
                min: 0.0,
                max: 1.0,
                smoothing_secs: 0.1,
                binding: None,
            },
            value: 0.5,
            target: 1.0,
        },
    )]);
    assert_eq!(
        layer_outputs(&layer, &params),
        LayerOutputs {
            volume_db: -9.0,
            pitch: 1.0,
            cutoff_hz: Some(400.0)
        }
    );
    assert_eq!(
        layer_outputs(&layer, &HashMap::new()),
        LayerOutputs {
            volume_db: -3.0,
            pitch: 1.0,
            cutoff_hz: None
        }
    );
}
