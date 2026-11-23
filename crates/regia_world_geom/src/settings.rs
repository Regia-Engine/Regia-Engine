// SPDX-License-Identifier: MPL-2.0
//! `GeomSettings` (spec 121 § 5.1, § 7): DO27's caps as settings, cooked into the canonical section.

use serde::{Deserialize, Serialize};

/// The most spans `SpanBuf` holds (spec 121 § 5.1); `max_floors` may not exceed it.
pub const SPAN_CAPACITY: u8 = 16;

/// Settings with DO27's values as defaults (ADR-8012 A-1: caps become settings). Cooked into the
/// canonical section of the file (spec 121 § 9.2), so `content_hash` covers them: a peer with other
/// settings has another content hash and fails the match handshake (DO27 sign-off E6). The cook
/// refuses a non-finite value and writes `-0.0` as `+0.0` ([`GeomSettings::normalized`]), so equal
/// settings always have equal bytes (ADR-8084).
///
/// In `<map>.meshlayers.ron` every field may be omitted; an omitted field takes its default.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GeomSettings {
    /// Query index cell, metres (DO27: 16.0). A performance setting, never a sim bit.
    pub index_cell_m: f32,
    /// Wall triangles are filed this much wider so a disc of up to this radius finds every wall
    /// (DO27: 8.0). Also the largest `push_disc` radius.
    pub wall_pad_m: f32,
    /// Floors closer than this are one floor (T3; DO27: 1e-3).
    pub same_height_m: f32,
    /// A span's ceiling is the lowest lid more than this over its floor (T3; DO27: 0.05).
    pub lid_gap_m: f32,
    /// A layer span with less clearance than this (and a lid) carries no placed body (DO27: 2.5).
    pub hull_fit_m: f32,
    /// Largest per-tick floor step the query may report at full speed, millimetres (acceptance
    /// only; default 50, DO27 sign-off E3).
    pub step_bound_mm: i32,
    /// Floors kept per column in `floors_at` (default 8; at most [`SPAN_CAPACITY`]).
    pub max_floors: u8,
    /// Minimum rock kept over a layer roof by a ground edit, millimetres (default 2000).
    pub min_cover_mm: i32,
    /// Nested-pitch fallback: the most a coarser tile's ground may differ from the full-pitch
    /// ground, metres (DO27's hop gate: 0.05).
    pub pitch_error_m: f32,
    /// Nested-pitch fallback: no pitch border within this distance of a mouth ring or a layer
    /// footprint, metres (default 16.0).
    pub fine_band_m: f32,
}

impl Default for GeomSettings {
    fn default() -> Self {
        Self {
            index_cell_m: 16.0,
            wall_pad_m: 8.0,
            same_height_m: 1e-3,
            lid_gap_m: 0.05,
            hull_fit_m: 2.5,
            step_bound_mm: 50,
            max_floors: 8,
            min_cover_mm: 2000,
            pitch_error_m: 0.05,
            fine_band_m: 16.0,
        }
    }
}

/// Why settings were refused (agent code `geom_bad_settings`).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("geom setting `{field}` {problem}")]
pub struct SettingsError {
    /// The field name as written in the file.
    pub field: &'static str,
    /// What is wrong with it.
    pub problem: &'static str,
}

impl SettingsError {
    /// The agent error code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        "geom_bad_settings"
    }
}

/// `-0.0` becomes `+0.0`; every other value keeps its bits.
fn plus_zero(v: f32) -> f32 {
    if v == 0.0 { 0.0 } else { v }
}

impl GeomSettings {
    /// The settings as the cook writes them: every `-0.0` as `+0.0`, refused when a float is not
    /// finite or a value is out of its range (`index_cell_m` and `pitch_error_m` above 0, the other
    /// lengths at least 0, `max_floors` in 1..=16).
    ///
    /// # Errors
    ///
    /// [`SettingsError`] names the first bad field, in declaration order.
    pub fn normalized(&self) -> Result<GeomSettings, SettingsError> {
        let floats: [(&'static str, f32, bool); 7] = [
            ("index_cell_m", self.index_cell_m, true),
            ("wall_pad_m", self.wall_pad_m, false),
            ("same_height_m", self.same_height_m, false),
            ("lid_gap_m", self.lid_gap_m, false),
            ("hull_fit_m", self.hull_fit_m, false),
            ("pitch_error_m", self.pitch_error_m, true),
            ("fine_band_m", self.fine_band_m, false),
        ];
        for (field, v, positive) in floats {
            if !v.is_finite() {
                return Err(SettingsError {
                    field,
                    problem: "is not finite",
                });
            }
            if positive && v <= 0.0 {
                return Err(SettingsError {
                    field,
                    problem: "must be above 0",
                });
            }
            if v < 0.0 {
                return Err(SettingsError {
                    field,
                    problem: "must not be negative",
                });
            }
        }
        if self.step_bound_mm < 0 {
            return Err(SettingsError {
                field: "step_bound_mm",
                problem: "must not be negative",
            });
        }
        if self.max_floors == 0 || self.max_floors > SPAN_CAPACITY {
            return Err(SettingsError {
                field: "max_floors",
                problem: "must be in 1..=16",
            });
        }
        if self.min_cover_mm < 0 {
            return Err(SettingsError {
                field: "min_cover_mm",
                problem: "must not be negative",
            });
        }
        Ok(GeomSettings {
            index_cell_m: plus_zero(self.index_cell_m),
            wall_pad_m: plus_zero(self.wall_pad_m),
            same_height_m: plus_zero(self.same_height_m),
            lid_gap_m: plus_zero(self.lid_gap_m),
            hull_fit_m: plus_zero(self.hull_fit_m),
            step_bound_mm: self.step_bound_mm,
            max_floors: self.max_floors,
            min_cover_mm: self.min_cover_mm,
            pitch_error_m: plus_zero(self.pitch_error_m),
            fine_band_m: plus_zero(self.fine_band_m),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_do27s_values_and_normalize_to_themselves() {
        let d = GeomSettings::default();
        assert_eq!(d.index_cell_m, 16.0);
        assert_eq!(d.max_floors, 8);
        assert_eq!(d.normalized(), Ok(d));
    }

    #[test]
    fn negative_zero_becomes_positive_zero_and_bad_values_are_named() {
        let s = GeomSettings {
            wall_pad_m: -0.0,
            ..GeomSettings::default()
        };
        assert_eq!(s.normalized().map(|n| n.wall_pad_m.to_bits()), Ok(0));
        let nan = GeomSettings {
            lid_gap_m: f32::NAN,
            ..GeomSettings::default()
        };
        assert_eq!(nan.normalized().map_err(|e| e.field), Err("lid_gap_m"));
        let zero_cell = GeomSettings {
            index_cell_m: 0.0,
            ..GeomSettings::default()
        };
        assert_eq!(zero_cell.normalized().map_err(|e| e.field), Err("index_cell_m"));
        let floors = GeomSettings {
            max_floors: 17,
            ..GeomSettings::default()
        };
        assert_eq!(floors.normalized().map_err(|e| e.field), Err("max_floors"));
    }
}
