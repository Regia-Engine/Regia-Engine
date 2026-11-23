// SPDX-License-Identifier: MIT-0
//! The hitch soak on a DO27 content bundle named on the command line (`do27_chase --content <dir>`):
//! a read-only layout check, deterministic camera paths and a per-frame hitch CSV.
//!
//! The bundle is DO27's content drop (`<dir>/<map>/level.ron` and the files beside it, or a
//! DO27 content root with `<dir>/maps/<map>/level.ron`, which is what the chase loads). Nothing
//! here writes into it: [`write_hitch_csv`] refuses an output path inside the bundle. The camera
//! paths are pure functions of the frame number, so a before/after pair of runs differs only by
//! the engine.

use std::f32::consts::TAU;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::LookdevError;
use crate::chase::{ChaseSample, DT};
use crate::soak::{SoakCut, event_tags};

/// A frame is a hitch when its wall time exceeds this multiple of the run's median.
pub const HITCH_RATIO: f32 = 2.0;

/// A DO27 content bundle: a directory of per-map folders, each holding a `level.ron`.
///
/// The DO27 content root (`REGIA_DO27_CONTENT`, whose maps sit in `maps/<map>/`) is a bundle
/// too: the chase loads its maps from `<content>/maps/<map>`, so both spellings must open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    root: PathBuf,
    maps: Vec<String>,
}

/// The sorted names of the folders of `dir` that hold a `level.ron`.
fn level_folders(dir: &Path) -> Result<Vec<String>, LookdevError> {
    let entries = std::fs::read_dir(dir).map_err(|source| LookdevError::Io {
        path: dir.display().to_string(),
        source,
    })?;
    let mut maps: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().join("level.ron").is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    maps.sort();
    Ok(maps)
}

impl Bundle {
    /// Checks the layout of `root` (read-only) and lists its maps, sorted by name.
    ///
    /// The maps are the `<root>/<map>/level.ron` folders, or, when there are none, the
    /// `<root>/maps/<map>/level.ron` folders of a DO27 content root. Either way the whole of
    /// `root` is the read-only bundle.
    ///
    /// # Errors
    ///
    /// [`LookdevError::Config`] when `root` is not a directory or holds neither
    /// `<map>/level.ron` nor `maps/<map>/level.ron`; [`LookdevError::Io`] when the directory
    /// cannot be read.
    pub fn open(root: &Path) -> Result<Self, LookdevError> {
        if !root.is_dir() {
            return Err(LookdevError::Config(format!(
                "content bundle {} is not a directory",
                root.display()
            )));
        }
        let mut maps = level_folders(root)?;
        let nested = root.join("maps");
        if maps.is_empty() && nested.is_dir() {
            maps = level_folders(&nested)?;
        }
        if maps.is_empty() {
            return Err(LookdevError::Config(format!(
                "content bundle {} has no <map>/level.ron and no maps/<map>/level.ron",
                root.display()
            )));
        }
        Ok(Self {
            root: root.to_path_buf(),
            maps,
        })
    }

    /// The bundle directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The maps found, sorted by name.
    #[must_use]
    pub fn maps(&self) -> &[String] {
        &self.maps
    }

    /// Whether the bundle holds the map named `name`.
    #[must_use]
    pub fn has_map(&self, name: &str) -> bool {
        self.maps.iter().any(|m| m == name)
    }

    /// `Err` when `out` lies inside the bundle: the bundle is read-only.
    ///
    /// # Errors
    ///
    /// [`LookdevError::Config`] when `out` is under the bundle root.
    pub fn ensure_outside(&self, out: &Path) -> Result<(), LookdevError> {
        let absolute = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
        if absolute(out).starts_with(absolute(&self.root)) {
            return Err(LookdevError::Config(format!(
                "{} is inside the content bundle, which is read-only",
                out.display()
            )));
        }
        Ok(())
    }
}

/// A deterministic camera path over a square map of half-width `half` metres.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraPath {
    /// A circle at 0.8 of the half-width, looking at the map centre.
    Orbit,
    /// A back-and-forth pass along the X axis, looking along +Z.
    Line,
    /// A lemniscate over the map, looking at the centre.
    Figure8,
}

impl CameraPath {
    /// Frames per lap of every path (30 s at 60 Hz).
    pub const LAP_FRAMES: u32 = 1800;
    /// Eye height above the map origin, metres.
    pub const EYE_HEIGHT_M: f32 = 60.0;
    /// Horizontal field of view of the path cameras, degrees.
    pub const FOV_DEG: f32 = 60.0;

    /// Parses `orbit`, `line` or `figure8`.
    ///
    /// # Errors
    ///
    /// [`LookdevError::Config`] for any other name.
    pub fn parse(name: &str) -> Result<Self, LookdevError> {
        match name {
            "orbit" => Ok(Self::Orbit),
            "line" => Ok(Self::Line),
            "figure8" => Ok(Self::Figure8),
            other => Err(LookdevError::Config(format!(
                "--camera-path {other}: orbit|line|figure8"
            ))),
        }
    }

    /// The `(eye, target)` of `frame`; the same inputs always give the same pose.
    #[must_use]
    pub fn pose(self, frame: u32, half: f32) -> ([f32; 3], [f32; 3]) {
        let phase = (frame % Self::LAP_FRAMES) as f32 / Self::LAP_FRAMES as f32;
        let a = phase * TAU;
        let h = Self::EYE_HEIGHT_M;
        match self {
            Self::Orbit => {
                let r = 0.8 * half;
                ([r * a.cos(), h, r * a.sin()], [0.0, 0.0, 0.0])
            }
            Self::Line => {
                let x = 0.8 * half * a.sin();
                ([x, h, -0.8 * half], [x, 0.0, 0.0])
            }
            Self::Figure8 => {
                let r = 0.8 * half;
                ([r * a.sin(), h, 0.5 * r * (2.0 * a).sin()], [0.0, 0.0, 0.0])
            }
        }
    }

    /// `count` fixed cameras spaced evenly over one lap, as soak cuts.
    #[must_use]
    pub fn waypoints(self, count: u32, half: f32) -> Vec<SoakCut> {
        (0..count)
            .map(|i| {
                let (eye, target) = self.pose(i * Self::LAP_FRAMES / count.max(1), half);
                SoakCut::Fixed(eye, target, Self::FOV_DEG)
            })
            .collect()
    }
}

/// The per-frame hitch CSV: `frame,t_s,wall_ms,ratio,hitch,events` with `ratio` the wall time over
/// `p50_ms` and `hitch` 1 when it exceeds [`HITCH_RATIO`].
#[must_use]
pub fn hitch_csv(samples: &[ChaseSample], p50_ms: f32) -> String {
    let mut out = String::from("frame,t_s,wall_ms,ratio,hitch,events\n");
    let p50 = p50_ms.max(1e-6);
    for (i, s) in samples.iter().enumerate() {
        let ratio = s.wall_ms / p50;
        let _ = writeln!(
            out,
            "{i},{:.4},{:.3},{ratio:.3},{},{}",
            i as f32 * DT,
            s.wall_ms,
            u8::from(ratio > HITCH_RATIO),
            event_tags(s.events).replace(',', "+"),
        );
    }
    out
}

/// Writes [`hitch_csv`] to `path` (temp file, then rename) after checking it is outside `bundle`.
///
/// # Errors
///
/// [`LookdevError::Config`] when `path` is inside the bundle; [`LookdevError::Io`] when the
/// file cannot be written.
pub fn write_hitch_csv(
    bundle: &Bundle,
    path: &Path,
    samples: &[ChaseSample],
    p50_ms: f32,
) -> Result<(), LookdevError> {
    bundle.ensure_outside(path)?;
    let io = |source: std::io::Error| LookdevError::Io {
        path: path.display().to_string(),
        source,
    };
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, hitch_csv(samples, p50_ms)).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}
