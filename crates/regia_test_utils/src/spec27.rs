// SPDX-License-Identifier: MPL-2.0
//! Helpers of the `specs/27` § 2.2 calibration suite (ADR-2660,
//! `crates/regia_pt/tests/spec27_calibration.rs`): one record per § 2.2 target, the report that
//! lists them, and the metrics the suite's new rows need.
//!
//! - Rows: every scoring test writes one [`Row`] (target, measured value, verdict, scene, samples
//!   per pixel) to `$REGIA_SPEC27_OUT/rows/<id>.tsv`; [`render_report`] turns the rows into the
//!   evidence page `docs/evidence/spec27/<date>-<adapter>.md`. Rows owned by older tests
//!   (`gi_validation`, `sssr_specular`, `aerial_parity`, `cloud_field`) are read from their logs
//!   with [`parse_after`] and [`test_result`].
//! - Metrics: CIEDE2000 colour difference ([`ciede2000`], Sharma, Wu, Dalal 2005), CIELAB from
//!   display sRGB ([`srgb_to_lab`]), the 10–90 % width of a shadow edge ([`edge_width`]), the
//!   exposure offset that maps one display value onto another ([`ev_offset`]) and the centroid
//!   of a light shaft's profile ([`centroid`]).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Directory the suite writes its rows (`rows/`), logs and plates to.
pub const OUT_ENV: &str = "REGIA_SPEC27_OUT";

/// Commit the report names (`git rev-parse --short HEAD` when unset).
pub const COMMIT_ENV: &str = "REGIA_SPEC27_COMMIT";

/// The § 2.2 row ids in the table's order (the report lists rows in this order, unknown ids
/// last).
pub const ROW_ORDER: &[&str] = &[
    "exposure_mid_grey",
    "exposure_colorchecker",
    "direct_rel_rmse",
    "direct_flip",
    "sun_shadow_flip",
    "sun_penumbra_1m",
    "sun_penumbra_5m",
    "sun_penumbra_20m",
    "sun_contact_gap",
    "local_shadow_flip_point",
    "local_shadow_flip_spot",
    "local_shadow_present",
    "gi_diffuse_flip_trench",
    "gi_diffuse_flip_color_room",
    "gi_diffuse_p95",
    "gi_leak_sealed_box",
    "specular_flip",
    "specular_mirror_floor",
    "ao_contact",
    "atmosphere_sky_bands",
    "atmosphere_aerial",
    "fog_flip",
    "fog_shaft_position",
    "terrain_normal",
    "decal_aovs",
    "decal_flip",
    "clouds",
];

/// Outcome of one row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The measurement meets the target.
    Pass,
    /// The measurement misses the target.
    Fail,
    /// Measured, but the scene or metric is known to be incomplete (named in the note).
    Provisional,
    /// No automatic metric on the lane scenes yet (the note says why).
    NotMeasured,
}

impl Verdict {
    /// `Pass` when `pass`, else `Fail`.
    #[must_use]
    pub fn of(pass: bool) -> Self {
        if pass { Self::Pass } else { Self::Fail }
    }

    /// Lowercase name used in the row files and the report.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Provisional => "provisional",
            Self::NotMeasured => "not measured",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        [Self::Pass, Self::Fail, Self::Provisional, Self::NotMeasured]
            .into_iter()
            .find(|v| v.name() == name)
    }
}

/// One scored § 2.2 target.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// Stable id ([`ROW_ORDER`]).
    pub id: String,
    /// The § 2.2 area (`Sun shadows`, …).
    pub area: String,
    /// The target as § 2.2 states it.
    pub target: String,
    /// The measured value, with units.
    pub measured: String,
    /// Pass, fail, provisional or not measured.
    pub verdict: Verdict,
    /// Scene (§ 3.2 name or the suite's own fixture).
    pub scene: String,
    /// Path-tracer samples per pixel of the reference (`-` when none).
    pub spp: String,
    /// How far past the target a failing row is, as a factor ≥ 1 (measured over target, or the
    /// inverse for a lower bound); ranks the fixes. `None` for passing or unmeasured rows.
    pub miss: Option<f64>,
    /// Method and caveats.
    pub note: String,
    /// The commit the row was measured at when it differs from the report's (a row carried over
    /// from an earlier page, [`rows_from_report`]); `None` = the report's commit (ADR-2951).
    pub commit: Option<String>,
}

fn clean(field: &str) -> String {
    field.replace(['\t', '\n', '\r'], " ")
}

impl Row {
    /// One tab-separated line (tabs and newlines inside fields become spaces); a tenth field
    /// carries [`Self::commit`] when set.
    #[must_use]
    pub fn to_line(&self) -> String {
        let mut fields = vec![
            clean(&self.id),
            clean(&self.area),
            clean(&self.target),
            clean(&self.measured),
            self.verdict.name().to_owned(),
            clean(&self.scene),
            clean(&self.spp),
            self.miss.map_or_else(|| "-".to_owned(), |m| format!("{m:.3}")),
            clean(&self.note),
        ];
        if let Some(c) = &self.commit {
            fields.push(clean(c));
        }
        fields.join("\t")
    }

    /// Parses [`Self::to_line`].
    #[must_use]
    pub fn from_line(line: &str) -> Option<Self> {
        let f: Vec<&str> = line.trim_end_matches(['\n', '\r']).split('\t').collect();
        if f.len() != 9 && f.len() != 10 {
            return None;
        }
        Some(Self {
            id: f[0].to_owned(),
            area: f[1].to_owned(),
            target: f[2].to_owned(),
            measured: f[3].to_owned(),
            verdict: Verdict::from_name(f[4])?,
            scene: f[5].to_owned(),
            spp: f[6].to_owned(),
            miss: f[7].parse().ok(),
            note: f[8].to_owned(),
            commit: f.get(9).map(|c| (*c).to_owned()),
        })
    }
}

/// The rows of an evidence page [`render_report`] wrote, each with the commit its table cell
/// names and the miss its "Failing rows" entry gives, so a later run can carry the rows it does
/// not re-measure over into its page (ADR-2951). Lines that are not table rows are skipped.
#[must_use]
pub fn rows_from_report(md: &str) -> Vec<Row> {
    let strip = |c: &str| c.trim().trim_matches('`').trim_matches('*').to_owned();
    let mut rows: Vec<Row> = md
        .lines()
        .filter(|l| l.starts_with("| ") && !l.starts_with("| Area ") && !l.starts_with("|---"))
        .filter_map(|l| {
            let cells: Vec<&str> = l
                .trim()
                .trim_start_matches('|')
                .trim_end_matches('|')
                .split(" | ")
                .collect();
            if cells.len() != 9 {
                return None;
            }
            Some(Row {
                id: strip(cells[1]),
                area: cells[0].trim().to_owned(),
                target: cells[2].trim().to_owned(),
                measured: cells[3].trim().to_owned(),
                verdict: Verdict::from_name(&strip(cells[4]))?,
                scene: cells[5].trim().to_owned(),
                spp: cells[6].trim().to_owned(),
                miss: None,
                note: cells[8].trim().to_owned(),
                commit: Some(strip(cells[7])),
            })
        })
        .collect();
    for line in md.lines() {
        let (Some(open), Some(at)) = (line.find('`'), line.rfind("(miss ")) else {
            continue;
        };
        let Some(close) = line[open + 1..].find('`') else {
            continue;
        };
        let id = &line[open + 1..open + 1 + close];
        let value = line[at + 6..].trim_end_matches(").").trim_end_matches('×');
        if let (Ok(m), Some(r)) = (value.parse::<f64>(), rows.iter_mut().find(|r| r.id == id)) {
            r.miss = Some(m);
        }
    }
    rows
}

/// The miss factor of a value against an upper bound (`value ≤ bound` passes): `None` when it
/// passes, else `value / bound`.
#[must_use]
pub fn miss_over(value: f64, bound: f64) -> Option<f64> {
    (value > bound).then(|| if bound > 0.0 { value / bound } else { f64::INFINITY })
}

/// `$REGIA_SPEC27_OUT`, when set.
#[must_use]
pub fn out_dir() -> Option<PathBuf> {
    regia_core::brand::env_var_os(OUT_ENV).map(PathBuf::from)
}

/// Prints `row` and, with `$REGIA_SPEC27_OUT` set, writes it to `rows/<id>.tsv` (atomically).
///
/// # Errors
///
/// Creating the directory or writing the file failed.
pub fn record(row: &Row) -> std::io::Result<()> {
    eprintln!(
        "spec27 {}: {} (target {}) -> {} [{}]",
        row.id,
        row.measured,
        row.target,
        row.verdict.name(),
        row.scene
    );
    let Some(dir) = out_dir() else {
        return Ok(());
    };
    let rows = dir.join("rows");
    std::fs::create_dir_all(&rows)?;
    crate::write_atomic(&rows.join(format!("{}.tsv", row.id)), row.to_line().as_bytes())
}

/// Every row under `dir/rows`, in [`ROW_ORDER`]. Unreadable or malformed files are skipped.
#[must_use]
pub fn read_rows(dir: &Path) -> Vec<Row> {
    let Ok(entries) = std::fs::read_dir(dir.join("rows")) else {
        return Vec::new();
    };
    let mut rows: Vec<Row> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "tsv"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|text| Row::from_line(&text))
        .collect();
    let rank = |id: &str| ROW_ORDER.iter().position(|r| *r == id).unwrap_or(ROW_ORDER.len());
    rows.sort_by(|a, b| rank(&a.id).cmp(&rank(&b.id)).then_with(|| a.id.cmp(&b.id)));
    rows
}

/// The evidence page: a header naming the adapter, commit and date, one table row per target,
/// and the failing rows ranked by the size of their miss.
#[must_use]
pub fn render_report(rows: &[Row], adapter: &str, commit: &str, date: &str) -> String {
    let mut s = format!(
        "# `specs/27` § 2.2 calibration: {date}\n\n\
         Adapter: **{adapter}**. Numbers measured on this adapter are labelled with it; a lavapipe \
         number is a lavapipe number, not a card number.\n\nCommit: `{commit}` (a row carried over from an earlier run names its own in the Commit column). Suite: \
         `crates/regia_pt/tests/spec27_calibration.rs` (ADR-2660; how to run it: `docs/06` § 16, \
         `book/agents/spec27-calibration.md`).\n\n\
         | Area | Row | Target | Measured | Result | Scene | spp | Commit | Note |\n\
         |---|---|---|---|---|---|---|---|---|\n"
    );
    for r in rows {
        let _ = writeln!(
            s,
            "| {} | `{}` | {} | {} | **{}** | {} | {} | `{}` | {} |",
            r.area,
            r.id,
            r.target,
            r.measured,
            r.verdict.name(),
            r.scene,
            r.spp,
            r.commit.as_deref().unwrap_or(commit),
            r.note
        );
    }
    // Provisional rows that miss their target are ranked too, marked as such.
    let mut failing: Vec<&Row> = rows
        .iter()
        .filter(|r| r.verdict == Verdict::Fail || (r.verdict == Verdict::Provisional && r.miss.is_some()))
        .collect();
    failing.sort_by(|a, b| {
        b.miss
            .unwrap_or(0.0)
            .partial_cmp(&a.miss.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    s.push_str("\n## Failing rows, largest miss first\n\n");
    if failing.is_empty() {
        s.push_str("None.\n");
    }
    for (i, r) in failing.iter().enumerate() {
        let miss = r.miss.map_or_else(|| "–".to_owned(), |m| format!("{m:.2}×"));
        let provisional = if r.verdict == Verdict::Provisional {
            ", provisional"
        } else {
            ""
        };
        let _ = writeln!(
            s,
            "{}. `{}` ({}{provisional}): {} against {} (miss {miss}).",
            i + 1,
            r.id,
            r.area,
            r.measured,
            r.target
        );
    }
    let missing: Vec<&str> = ROW_ORDER
        .iter()
        .copied()
        .filter(|id| !rows.iter().any(|r| r.id == *id))
        .collect();
    if !missing.is_empty() {
        let _ = writeln!(
            s,
            "\n## Rows with no record in this run\n\n{}",
            missing
                .iter()
                .map(|m| format!("`{m}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    s
}

/// The first number after `prefix` in `log` (the last occurrence of the prefix wins, so a
/// rerun appended to the same log reports its own value).
#[must_use]
pub fn parse_after(log: &str, prefix: &str) -> Option<f64> {
    let at = log.rfind(prefix)? + prefix.len();
    let rest = log[at..].trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e')))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// `(passed, failed)` from the last `test result:` line of a `cargo test` log.
#[must_use]
pub fn test_result(log: &str) -> Option<(u32, u32)> {
    let line = log.lines().rev().find(|l| l.starts_with("test result:"))?;
    let count = |word: &str| -> Option<u32> {
        let at = line.find(word)?;
        line[..at]
            .split_whitespace()
            .last()?
            .trim_matches(';')
            .parse()
            .ok()
    };
    Some((count(" passed")?, count(" failed")?))
}

/// Decodes one sRGB-encoded channel to linear.
#[must_use]
pub fn srgb_decode(v: f64) -> f64 {
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// CIELAB (D65 white) of a display-encoded sRGB colour in `[0, 1]`.
#[must_use]
pub fn srgb_to_lab(rgb: [f32; 3]) -> [f64; 3] {
    let [r, g, b] = rgb.map(|c| srgb_decode(f64::from(c.clamp(0.0, 1.0))));
    let x = 0.412_456_4 * r + 0.357_576_1 * g + 0.180_437_5 * b;
    let y = 0.212_672_9 * r + 0.715_152_2 * g + 0.072_175_0 * b;
    let z = 0.019_333_9 * r + 0.119_192_0 * g + 0.950_304_1 * b;
    let d = 6.0 / 29.0;
    let f = |t: f64| {
        if t > d * d * d {
            t.cbrt()
        } else {
            t / (3.0 * d * d) + 4.0 / 29.0
        }
    };
    let (fx, fy, fz) = (f(x / 0.950_47), f(y), f(z / 1.088_83));
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

/// CIEDE2000 colour difference of two CIELAB colours (`kL = kC = kH = 1`), from Sharma, Wu and
/// Dalal, "The CIEDE2000 Color-Difference Formula: Implementation Notes, Supplementary Test
/// Data, and Mathematical Observations", Color Research & Application 30 (2005), eqs. 1–17.
#[must_use]
pub fn ciede2000(lab1: [f64; 3], lab2: [f64; 3]) -> f64 {
    let [l1, a1, b1] = lab1;
    let [l2, a2, b2] = lab2;
    let pow7 = |v: f64| v.powi(7);
    let c_bar = 0.5 * (a1.hypot(b1) + a2.hypot(b2));
    let g = 0.5 * (1.0 - (pow7(c_bar) / (pow7(c_bar) + pow7(25.0))).sqrt());
    let (a1p, a2p) = ((1.0 + g) * a1, (1.0 + g) * a2);
    let (c1p, c2p) = (a1p.hypot(b1), a2p.hypot(b2));
    let hue = |b: f64, a: f64| {
        if b == 0.0 && a == 0.0 {
            0.0
        } else {
            b.atan2(a).to_degrees().rem_euclid(360.0)
        }
    };
    let (h1p, h2p) = (hue(b1, a1p), hue(b2, a2p));
    let dl = l2 - l1;
    let dc = c2p - c1p;
    let chroma_zero = c1p * c2p == 0.0;
    let dh_angle = if chroma_zero {
        0.0
    } else {
        let d = h2p - h1p;
        if d > 180.0 {
            d - 360.0
        } else if d < -180.0 {
            d + 360.0
        } else {
            d
        }
    };
    let dh = 2.0 * (c1p * c2p).sqrt() * (0.5 * dh_angle).to_radians().sin();
    let l_bar = 0.5 * (l1 + l2);
    let c_bar_p = 0.5 * (c1p + c2p);
    let h_bar = if chroma_zero {
        h1p + h2p
    } else if (h1p - h2p).abs() <= 180.0 {
        0.5 * (h1p + h2p)
    } else if h1p + h2p < 360.0 {
        0.5 * (h1p + h2p + 360.0)
    } else {
        0.5 * (h1p + h2p - 360.0)
    };
    let cos_d = |deg: f64| deg.to_radians().cos();
    let t = 1.0 - 0.17 * cos_d(h_bar - 30.0) + 0.24 * cos_d(2.0 * h_bar) + 0.32 * cos_d(3.0 * h_bar + 6.0)
        - 0.20 * cos_d(4.0 * h_bar - 63.0);
    let d_theta = 30.0 * (-((h_bar - 275.0) / 25.0).powi(2)).exp();
    let r_c = 2.0 * (pow7(c_bar_p) / (pow7(c_bar_p) + pow7(25.0))).sqrt();
    let l50 = (l_bar - 50.0).powi(2);
    let s_l = 1.0 + 0.015 * l50 / (20.0 + l50).sqrt();
    let s_c = 1.0 + 0.045 * c_bar_p;
    let s_h = 1.0 + 0.015 * c_bar_p * t;
    let r_t = -(2.0 * d_theta).to_radians().sin() * r_c;
    let (tl, tc, th) = (dl / s_l, dc / s_c, dh / s_h);
    (tl * tl + tc * tc + th * th + r_t * tc * th).max(0.0).sqrt()
}

/// The 24 patches of the ColorChecker chart as the manufacturer publishes them in sRGB
/// (D65, 8 bit), row by row from dark skin to black.
pub const COLORCHECKER_SRGB: [(&str, [u8; 3]); 24] = [
    ("dark skin", [115, 82, 68]),
    ("light skin", [194, 150, 130]),
    ("blue sky", [98, 122, 157]),
    ("foliage", [87, 108, 67]),
    ("blue flower", [133, 128, 177]),
    ("bluish green", [103, 189, 170]),
    ("orange", [214, 126, 44]),
    ("purplish blue", [80, 91, 166]),
    ("moderate red", [193, 90, 99]),
    ("purple", [94, 60, 108]),
    ("yellow green", [157, 188, 64]),
    ("orange yellow", [224, 163, 46]),
    ("blue", [56, 61, 150]),
    ("green", [70, 148, 73]),
    ("red", [175, 54, 60]),
    ("yellow", [231, 199, 31]),
    ("magenta", [187, 86, 149]),
    ("cyan", [8, 133, 161]),
    ("white", [243, 243, 242]),
    ("neutral 8", [200, 200, 200]),
    ("neutral 6.5", [160, 160, 160]),
    ("neutral 5", [122, 122, 121]),
    ("neutral 3.5", [85, 85, 85]),
    ("black", [52, 52, 52]),
];

/// Linear reflectance of a ColorChecker patch (its sRGB value decoded).
#[must_use]
pub fn colorchecker_linear(srgb: [u8; 3]) -> [f32; 3] {
    srgb.map(|c| srgb_decode(f64::from(c) / 255.0) as f32)
}

/// Width, in samples, between the points where `profile` first reaches `lo` and first reaches
/// `hi` (linear interpolation between samples), read from its dark end: a profile that falls
/// is read backwards. The 10–90 % width of a shadow edge is `edge_width(v, 0.1, 0.9)` on a
/// visibility profile. `None` when the profile never crosses both levels.
#[must_use]
pub fn edge_width(profile: &[f32], lo: f32, hi: f32) -> Option<f32> {
    let n = profile.len();
    if n < 2 {
        return None;
    }
    let rising = profile[n - 1] >= profile[0];
    let at = |i: usize| if rising { profile[i] } else { profile[n - 1 - i] };
    let crossing = |level: f32| -> Option<f32> {
        (1..n).find(|&i| at(i) >= level && at(i - 1) < level).map(|i| {
            let (a, b) = (at(i - 1), at(i));
            (i - 1) as f32 + (level - a) / (b - a).max(1e-9)
        })
    };
    let (x_lo, x_hi) = (crossing(lo)?, crossing(hi)?);
    (x_hi >= x_lo).then_some(x_hi - x_lo)
}

/// The exposure offset Δ (EV) for which the increasing function `f(Δ)` reaches `target`,
/// by bisection over `[-8, 8]` EV (clamped to the interval's ends).
#[must_use]
pub fn ev_offset(target: f64, f: impl Fn(f64) -> f64) -> f64 {
    let (mut lo, mut hi) = (-8.0f64, 8.0f64);
    if f(lo) >= target {
        return lo;
    }
    if f(hi) <= target {
        return hi;
    }
    for _ in 0..60 {
        let mid = 0.5 * (lo + hi);
        if f(mid) < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// Centroid (in samples) of the part of `profile` above `floor`; `None` when nothing is.
#[must_use]
pub fn centroid(profile: &[f32], floor: f32) -> Option<f32> {
    let (mut sum, mut weight) = (0.0f64, 0.0f64);
    for (i, v) in profile.iter().enumerate() {
        let w = f64::from((v - floor).max(0.0));
        sum += w * i as f64;
        weight += w;
    }
    (weight > 0.0).then(|| (sum / weight) as f32)
}

/// Value at quantile `q` (0–1) of `values` (nearest rank; 0 for an empty slice).
#[must_use]
pub fn quantile(values: &[f32], q: f32) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(f32::total_cmp);
    let i = ((q.clamp(0.0, 1.0) * (v.len() - 1) as f32).round() as usize).min(v.len() - 1);
    v[i]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ciede2000_matches_the_published_test_pairs() {
        // Sharma, Wu, Dalal 2005, Table 1, pairs 1-3.
        let reference = [50.0, 0.0, -82.7485];
        for (lab, want) in [
            ([50.0, 2.6772, -79.7751], 2.0425),
            ([50.0, 3.1571, -77.2803], 2.8615),
            ([50.0, 2.8361, -74.0200], 3.4412),
        ] {
            let got = ciede2000(lab, reference);
            assert!((got - want).abs() < 1e-3, "{lab:?}: {got} vs {want}");
        }
        assert!(ciede2000(reference, reference).abs() < 1e-12);
    }

    #[test]
    fn white_is_l100_and_grey_is_neutral() {
        let [l, a, b] = srgb_to_lab([1.0; 3]);
        assert!(
            (l - 100.0).abs() < 0.01 && a.abs() < 0.01 && b.abs() < 0.01,
            "{l} {a} {b}"
        );
        let [_, a, b] = srgb_to_lab([0.5; 3]);
        assert!(a.abs() < 0.01 && b.abs() < 0.01);
    }

    #[test]
    fn edge_width_reads_a_ramp_from_either_side() {
        let ramp: Vec<f32> = (0..21).map(|i| (i as f32 / 20.0).clamp(0.0, 1.0)).collect();
        let w = edge_width(&ramp, 0.1, 0.9).expect("crosses");
        assert!((w - 16.0).abs() < 1e-4, "{w}");
        let falling: Vec<f32> = ramp.iter().rev().copied().collect();
        assert!((edge_width(&falling, 0.1, 0.9).expect("crosses") - 16.0).abs() < 1e-4);
        assert_eq!(edge_width(&[0.0, 0.05], 0.1, 0.9), None);
    }

    #[test]
    fn ev_offset_inverts_a_doubling() {
        let d = ev_offset(4.0, |ev| 2f64.powf(ev));
        assert!((d - 2.0).abs() < 1e-9, "{d}");
        assert_eq!(ev_offset(1e9, |ev| ev), 8.0);
    }

    #[test]
    fn centroid_finds_the_peak() {
        let p = [0.0, 0.0, 1.0, 3.0, 1.0, 0.0];
        assert!((centroid(&p, 0.0).expect("peak") - 3.0).abs() < 1e-6);
        assert_eq!(centroid(&[0.0; 4], 0.0), None);
    }

    #[test]
    fn rows_round_trip_and_rank_failures() {
        let row = Row {
            id: "sun_penumbra_1m".into(),
            area: "Sun shadows".into(),
            target: "within 20 %".into(),
            measured: "3.1×\tPT".into(),
            verdict: Verdict::Fail,
            scene: "wall".into(),
            spp: "256".into(),
            miss: Some(15.5),
            note: "n".into(),
            commit: None,
        };
        let back = Row::from_line(&row.to_line()).expect("parses");
        assert_eq!(back.measured, "3.1× PT");
        assert_eq!(back.verdict, Verdict::Fail);
        assert_eq!(back.miss, Some(15.5));
        let small = Row {
            id: "direct_flip".into(),
            miss: Some(1.2),
            ..row.clone()
        };
        let provisional = Row {
            id: "gi_diffuse_flip_trench".into(),
            verdict: Verdict::Provisional,
            miss: Some(6.0),
            ..row.clone()
        };
        let md = render_report(&[small, back, provisional], "llvmpipe", "abc", "2026-11-05");
        let big_at = md.find("1. `sun_penumbra_1m`").expect("ranked first");
        assert!(
            md[big_at..].contains("2. `gi_diffuse_flip_trench` (Sun shadows, provisional)"),
            "{md}"
        );
        assert!(md[big_at..].contains("3. `direct_flip`"), "{md}");
        assert!(md.contains("Rows with no record"));
    }

    /// ADR-2951: a page's rows parse back with their commit and miss; a carried-over row keeps
    /// its own commit in a later page.
    #[test]
    fn a_page_parses_back_into_its_rows() {
        let failing = Row {
            id: "fog_flip".into(),
            area: "Volumetric fog".into(),
            target: "FLIP ≤ 0.10 ★".into(),
            measured: "mean 0.4783, p95 0.961".into(),
            verdict: Verdict::Fail,
            scene: "fog room".into(),
            spp: "1024".into(),
            miss: Some(4.783),
            note: "froxel fog".into(),
            commit: None,
        };
        let passing = Row {
            id: "direct_flip".into(),
            verdict: Verdict::Pass,
            miss: None,
            commit: Some("1234abcd".into()),
            ..failing.clone()
        };
        let md = render_report(
            &[failing.clone(), passing.clone()],
            "llvmpipe",
            "8adff05b",
            "2026-11-05",
        );
        let back = rows_from_report(&md);
        assert_eq!(back.len(), 2);
        let fog = back.iter().find(|r| r.id == "fog_flip").expect("fog row");
        assert_eq!(fog.commit.as_deref(), Some("8adff05b"));
        assert_eq!(fog.miss, Some(4.78));
        assert_eq!(fog.measured, failing.measured);
        assert_eq!(fog.verdict, Verdict::Fail);
        let direct = back.iter().find(|r| r.id == "direct_flip").expect("direct row");
        assert_eq!(direct.commit.as_deref(), Some("1234abcd"));
        assert_eq!(direct.miss, None);
        // Carried into a later page, the row names its own commit.
        let later = render_report(&back, "llvmpipe", "deadbeef", "2026-11-06");
        assert!(later.contains("| `8adff05b` | froxel fog |"), "{later}");
        assert_eq!(Row::from_line(&fog.to_line()).as_ref(), Some(fog));
    }

    #[test]
    fn logs_parse() {
        let log = "x: FLIP 0.2\nmetal: SSSR 0.287, off 0.3\nmetal: SSSR 0.250, off\n\
                   test result: ok. 7 passed; 1 failed; 0 ignored";
        assert_eq!(parse_after(log, "SSSR"), Some(0.250));
        assert_eq!(parse_after(log, "nothing"), None);
        assert_eq!(test_result(log), Some((7, 1)));
        assert_eq!(miss_over(0.1, 0.05), Some(2.0));
        assert_eq!(miss_over(0.01, 0.05), None);
    }

    #[test]
    fn colorchecker_white_is_bright() {
        let w = colorchecker_linear(COLORCHECKER_SRGB[18].1);
        assert!(w.iter().all(|c| *c > 0.85 && *c < 0.95), "{w:?}");
        assert!((quantile(&[3.0, 1.0, 2.0], 0.5) - 2.0).abs() < 1e-6);
    }
}
