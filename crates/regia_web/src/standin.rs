// SPDX-License-Identifier: MPL-2.0
//! A generic stand-in product and view: the bring-up piece the browser glue runs against, and the fixture
//! of the player's tests. Two options sold together in all but one combination, a tilt cone, damping.

use serde_json::{Value, json};

use crate::WebError;
use crate::seams::{CameraPose, PickApplied, PointerClass, ProductModel, ViewModel, ViewValues};

const GROUPS: &[(&str, &[&str], bool)] = &[
    ("Material", &["Yellow Gold", "Rose Gold", "Titanium"], true),
    ("Gem", &["Clear", "Ruby", "Sapphire"], true),
    ("Gauge", &["16", "14"], false),
];

fn sold(material: &str, gem: &str) -> bool {
    !(material == "Titanium" && gem == "Ruby")
}

/// The stand-in product.
#[derive(Clone, Debug)]
pub struct StandInProduct {
    picks: [String; 3],
}

impl Default for StandInProduct {
    fn default() -> Self {
        Self::new()
    }
}

impl StandInProduct {
    /// A new stand-in on its first values.
    pub fn new() -> Self {
        StandInProduct {
            picks: ["Yellow Gold".into(), "Clear".into(), "16".into()],
        }
    }

    /// The pick of `group`.
    pub fn pick(&self, group: &str) -> Option<&str> {
        GROUPS
            .iter()
            .position(|g| g.0 == group)
            .map(|i| self.picks[i].as_str())
    }

    fn sold_with(&self, group: usize, value: &str) -> bool {
        let mut p = self.picks.clone();
        p[group] = value.to_string();
        sold(&p[0], &p[1])
    }
}

impl ProductModel for StandInProduct {
    fn options(&self) -> Value {
        Value::Array(
            GROUPS
                .iter()
                .enumerate()
                .map(|(i, (name, values, visual))| {
                    let values: Vec<Value> = values
                        .iter()
                        .map(|v| {
                            let ok = self.sold_with(i, v);
                            let mut row = json!({"value": v, "label": v, "available": ok});
                            if !ok {
                                row["reason"] = json!("not_sold_with");
                                row["conflicts"] = json!([if i == 0 { "Gem" } else { "Material" }]);
                            }
                            row
                        })
                        .collect();
                    json!({"group": name, "kind": "single", "current": self.picks[i], "visual": visual, "values": values})
                })
                .collect(),
        )
    }

    fn set_pick(&mut self, group: &str, value: &Value, adjust: bool) -> Result<PickApplied, WebError> {
        let i = GROUPS
            .iter()
            .position(|g| g.0 == group)
            .ok_or_else(|| WebError::new("pick_unknown_group", format!("no group {group:?}")))?;
        let value = value
            .as_str()
            .filter(|v| GROUPS[i].1.contains(v))
            .ok_or_else(|| WebError::new("pick_unknown_value", format!("{group} has no value {value}")))?;
        let mut adjusted = Vec::new();
        if !self.sold_with(i, value) {
            let other = if i == 0 { 1 } else { 0 };
            let fix = GROUPS[other].1.iter().find(|v| {
                let mut p = self.picks.clone();
                p[i] = value.to_string();
                p[other] = v.to_string();
                sold(&p[0], &p[1])
            });
            match (adjust, fix) {
                (true, Some(v)) => {
                    self.picks[other] = v.to_string();
                    adjusted.push(GROUPS[other].0.to_string());
                }
                _ => {
                    return Err(WebError::new(
                        "pick_not_sold",
                        format!("{group}={value} is not sold with the current picks"),
                    ));
                }
            }
        }
        self.picks[i] = value.to_string();
        Ok(PickApplied {
            adjusted,
            geometry_changed: i == 2,
        })
    }

    fn build(&self, view: &ViewValues) -> Result<Value, WebError> {
        Ok(json!({
            "schema": "jewelry.build/1",
            "product_key": "shared/stand-in",
            "picks": {"Material": self.picks[0], "Gem": self.picks[1], "Gauge": self.picks[2]},
            "resolution": {"orderable": true, "reasons": []},
            "camera": {"rig_preset": "front", "tilt_xy_deg": view.tilt, "spin_deg": view.spin, "pan_mm": view.pan, "zoom": view.zoom, "dof": false},
            "tier": "interactive",
        }))
    }

    fn incomplete(&self) -> bool {
        false
    }
}

const TILT_MAX: f32 = 30.0;
const TAU_S: f32 = 0.07;

/// The stand-in drag view: one pointer turns inside a ±30° cone, the wheel zooms, damped.
#[derive(Clone, Debug)]
pub struct StandInView {
    target: ViewValues,
    shown: ViewValues,
    viewport: [f32; 2],
    grab: Option<(u64, f32, f32)>,
}

impl Default for StandInView {
    fn default() -> Self {
        Self::new()
    }
}

fn clamp(mut v: ViewValues) -> ViewValues {
    let [x, y] = [finite(v.tilt[0]), finite(v.tilt[1])];
    let r = (x * x + y * y).sqrt();
    let s = if r > TILT_MAX { TILT_MAX / r } else { 1.0 };
    v.tilt = [x * s, y * s];
    v.spin = 0.0;
    v.pan = [0.0, 0.0];
    v.zoom = if v.zoom.is_finite() {
        v.zoom.clamp(0.5, 4.0)
    } else {
        1.0
    };
    v
}

fn finite(x: f32) -> f32 {
    if x.is_finite() { x } else { 0.0 }
}

impl StandInView {
    /// The default front view.
    pub fn new() -> Self {
        StandInView {
            target: ViewValues::default(),
            shown: ViewValues::default(),
            viewport: [1.0, 1.0],
            grab: None,
        }
    }

    fn settled(&self) -> bool {
        (self.target.tilt[0] - self.shown.tilt[0]).abs() < 0.01
            && (self.target.tilt[1] - self.shown.tilt[1]).abs() < 0.01
            && (self.target.zoom - self.shown.zoom).abs() < 0.001
    }
}

impl ViewModel for StandInView {
    fn pointer_down(&mut self, id: u64, _class: PointerClass, x: f32, y: f32, _t_ms: f64) {
        self.grab = Some((id, x, y));
    }

    fn pointer_move(&mut self, id: u64, x: f32, y: f32, _t_ms: f64) {
        if let Some((grab, ox, oy)) = self.grab
            && grab == id
        {
            let per_px = 2.0 * TILT_MAX / self.viewport[0].min(self.viewport[1]).max(1.0);
            self.target.tilt[0] += (x - ox) * per_px;
            self.target.tilt[1] += (y - oy) * per_px;
            self.target = clamp(self.target);
            self.grab = Some((id, x, y));
        }
    }

    fn pointer_up(&mut self, id: u64, _t_ms: f64) {
        if self.grab.is_some_and(|g| g.0 == id) {
            self.grab = None;
        }
    }

    fn pointer_cancel(&mut self, id: u64) {
        self.pointer_up(id, 0.0);
    }

    fn wheel(&mut self, lines: f32) {
        self.target.zoom *= 1.1_f32.powf(finite(lines));
        self.target = clamp(self.target);
    }

    fn set_viewport(&mut self, width: f32, height: f32) {
        self.viewport = [width, height];
    }

    fn update(&mut self, dt_s: f32) -> bool {
        if self.settled() {
            let moved = self.shown != self.target;
            self.shown = self.target;
            return moved;
        }
        let a = 1.0 - (-dt_s.max(0.0) / TAU_S).exp();
        self.shown.tilt[0] += (self.target.tilt[0] - self.shown.tilt[0]) * a;
        self.shown.tilt[1] += (self.target.tilt[1] - self.shown.tilt[1]) * a;
        self.shown.zoom += (self.target.zoom - self.shown.zoom) * a;
        if self.settled() {
            self.shown = self.target;
        }
        true
    }

    fn is_animating(&self) -> bool {
        self.shown != self.target
    }

    fn set_view(&mut self, view: ViewValues, immediate: bool) -> ViewValues {
        self.target = clamp(view);
        if immediate {
            self.shown = self.target;
        }
        self.target
    }

    fn reset(&mut self) {
        self.target = ViewValues::default();
    }

    fn shown(&self) -> ViewValues {
        self.shown
    }

    fn pose(&self) -> CameraPose {
        let (yaw, pitch) = (self.shown.tilt[0].to_radians(), self.shown.tilt[1].to_radians());
        let d = 60.0 / self.shown.zoom.max(0.01);
        CameraPose {
            eye: [
                -yaw.sin() * pitch.cos() * d,
                pitch.sin() * d,
                yaw.cos() * pitch.cos() * d,
            ],
            look_at: [0.0, 0.0, 0.0],
            up: [0.0, 1.0, 0.0],
            fov_y_rad: 0.35,
            near: d * 0.1,
            far: d * 4.0,
        }
    }
}
