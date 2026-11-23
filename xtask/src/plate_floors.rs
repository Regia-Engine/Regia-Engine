// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask plate-floors`: per-vendor PSNR floors of the golden plates (ADR-2089).
//!
//! A test run with `REGIA_PLATE_DIR=<dir>` records one `<plate>.plate` line per golden compare
//! (`regia_test_utils::golden`), on lavapipe and on real GPUs alike, with `REGIA_SKIP_GOLDENS=1`
//! too. This command holds those lines to the floors in `docs/ci/plate-floors.toml`:
//!
//! - `check` exits 0 when every floored plate is at or above its floor, 1 when one is under it or
//!   was not measured, and 2 when the vendor has no floors (not gated yet, so a gate script can tell
//!   "unmeasured" from "failed").
//! - `measure` writes a vendor's row from one or more runs of the suite.
//! - `attribute` explains a run's floor failures with a reference (lavapipe) run of the same build
//!   (ADR-5798): for each plate under its floor it scores the vendor's frame against the reference
//!   run's frame, and says whether the golden moved (the two frames still agree as well as the floor
//!   asks and the reference run is under its own floor), this vendor diverges, the change is shared
//!   by both runs and only this vendor's floor catches it, or the scores and frames contradict each
//!   other. Exit 0 when every failure is a moved golden (which the reference run's own `check`
//!   fails), 1 otherwise or when the reference run lacks the plate, 2 when the vendor has no floors.
//!   The vendor run's own directory is refused as its reference.
//!
//! Goldens stay lavapipe-blessed (ADR-025): a floor is a net for a look regression on one vendor,
//! not a second golden.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Args as ClapArgs, Subcommand};
use serde::{Deserialize, Serialize};

/// The floor file, relative to the workspace root.
pub const FLOORS_PATH: &str = "docs/ci/plate-floors.toml";

/// The `version` the floor file carries.
pub const FLOORS_VERSION: u32 = 1;

/// The vendors the floor file must name. One with no floors is reported as not gated.
pub const VENDORS: &[&str] = &["nvidia", "amd", "intel", "lavapipe"];

/// Default headroom under the worst measured run, in dB.
pub const DEFAULT_MARGIN_DB: f64 = 3.0;

/// The highest floor `measure` writes. Reproducing the golden exactly scores infinity, and a floor
/// that high would fail any change the SSIM compare still accepts; 40 dB is about 2.5 of 255 levels
/// of RMS error, under what a person sees and over what a look regression costs.
pub const MAX_FLOOR_DB: f64 = 40.0;

/// Written above the table, since a TOML round trip drops comments.
const HEADER: &str = "\
# Per-vendor PSNR floors (dB) of each golden plate against its lavapipe-blessed golden (ADR-2089).
# Written by `cargo xtask plate-floors measure`, read by `cargo xtask plate-floors check`; the policy
# is docs/06-testing-and-ci.md section 13. A floor is the worst measured run, less its run-to-run
# spread and a margin, capped at 40 dB. A vendor with no floors is not gated yet (check exits 2).

";

/// `docs/ci/plate-floors.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FloorFile {
    /// [`FLOORS_VERSION`].
    pub version: u32,
    /// Vendor name to its row.
    pub vendors: BTreeMap<String, VendorFloors>,
}

/// One vendor's floors.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VendorFloors {
    /// What was measured: the card, the driver, the date and the commit. Empty when unmeasured.
    #[serde(default)]
    pub measured: String,
    /// Plate name to its floor in dB.
    #[serde(default)]
    pub floors: BTreeMap<String, f64>,
}

impl FloorFile {
    /// Parses and validates the floor file's text.
    ///
    /// # Errors
    /// Bad TOML, another version, a required vendor missing, an unknown vendor, a floor that is not
    /// finite and in `(0, MAX_FLOOR_DB]`, or floors with no `measured` note.
    pub fn parse(text: &str) -> Result<Self> {
        let file: Self = toml::from_str(text).context("parsing the floor file")?;
        if file.version != FLOORS_VERSION {
            bail!(
                "floor file version {} (this xtask reads {FLOORS_VERSION})",
                file.version
            );
        }
        for vendor in VENDORS {
            if !file.vendors.contains_key(*vendor) {
                bail!("vendor {vendor} is missing (an unmeasured vendor has an empty row)");
            }
        }
        for (vendor, row) in &file.vendors {
            if !VENDORS.contains(&vendor.as_str()) {
                bail!("unknown vendor {vendor} (known: {})", VENDORS.join(", "));
            }
            if !row.floors.is_empty() && row.measured.is_empty() {
                bail!("vendor {vendor} has floors but no `measured` note");
            }
            for (plate, floor) in &row.floors {
                if !floor.is_finite() || *floor <= 0.0 || *floor > MAX_FLOOR_DB {
                    bail!("{vendor} {plate}: floor {floor} is outside (0, {MAX_FLOOR_DB}]");
                }
            }
        }
        Ok(file)
    }

    /// The file's text: the header, then the table.
    ///
    /// # Errors
    /// A failed TOML serialization.
    pub fn render(&self) -> Result<String> {
        Ok(format!("{HEADER}{}", toml::to_string_pretty(self)?))
    }
}

/// One plate's score in a run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlateScore {
    /// PSNR in dB, infinite when the frame equals the golden.
    Db(f64),
    /// The frame and the golden have different sizes.
    SizeDiffers,
}

/// Parses a `.plate` line (`regia_test_utils::golden::plate_line`) into its name and score.
///
/// # Errors
/// A line that does not have the four fields, or an unreadable score.
pub fn parse_plate_line(line: &str) -> Result<(String, PlateScore)> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let [plate, _size, score, _golden] = fields.as_slice() else {
        bail!("plate line {line:?} does not have four fields");
    };
    let score = match *score {
        "inf" => PlateScore::Db(f64::INFINITY),
        "size_differs" => PlateScore::SizeDiffers,
        text => PlateScore::Db(
            text.parse::<f64>()
                .with_context(|| format!("plate {plate}: score {text:?}"))?,
        ),
    };
    Ok(((*plate).to_string(), score))
}

/// Reads every `*.plate` file of a run directory.
///
/// # Errors
/// An unreadable directory or file, or a malformed line.
pub fn read_run(dir: &Path) -> Result<BTreeMap<String, PlateScore>> {
    let mut run = BTreeMap::new();
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("plate") {
            continue;
        }
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let (plate, score) = parse_plate_line(&text).with_context(|| format!("in {}", path.display()))?;
        run.insert(plate, score);
    }
    Ok(run)
}

/// What [`check`] found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CheckReport {
    /// Plates under their floor, or floored and not usable; one line each.
    pub failures: Vec<String>,
    /// Plates in the run with no floor for this vendor (a new golden, not gated).
    pub unfloored: Vec<String>,
    /// Floored plates the run did not record, when the run is `partial`.
    pub not_run: Vec<String>,
    /// Plates that met their floor.
    pub passed: usize,
}

/// Holds a run to a vendor's floors. A floored plate the run did not record fails, unless `partial`
/// (a run of part of the suite) says to list it instead. A partial run that matched no floor at
/// all still fails: it means the capture did not work, and would otherwise pass with nothing checked.
#[must_use]
pub fn check(
    run: &BTreeMap<String, PlateScore>,
    floors: &BTreeMap<String, f64>,
    partial: bool,
) -> CheckReport {
    let mut report = CheckReport::default();
    for (plate, floor) in floors {
        match run.get(plate) {
            None if partial => report.not_run.push(plate.clone()),
            None => report.failures.push(format!(
                "{plate}: not measured (floor {floor:.1} dB); a removed or renamed golden needs `plate-floors measure`"
            )),
            Some(PlateScore::SizeDiffers) => report
                .failures
                .push(format!("{plate}: frame and golden differ in size (floor {floor:.1} dB)")),
            Some(PlateScore::Db(db)) if db < floor => report
                .failures
                .push(format!("{plate}: {db:.2} dB is under the floor {floor:.1} dB")),
            Some(PlateScore::Db(_)) => report.passed += 1,
        }
    }
    report.unfloored = run.keys().filter(|p| !floors.contains_key(*p)).cloned().collect();
    if partial && !floors.is_empty() && report.passed == 0 && report.failures.is_empty() {
        report
            .failures
            .push("no floored plate was recorded: was REGIA_PLATE_DIR set for this run?".to_string());
    }
    report
}

/// The vendor whose runs bless the goldens (ADR-025): the reference of [`attribute`].
pub const REFERENCE_VENDOR: &str = "lavapipe";

/// Where [`attribute`] places a plate that is under a vendor's floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attribution {
    /// The golden is what moved: the reference run is under its own floor for the plate (so
    /// `plate-floors check --vendor lavapipe` on it fails the plate), and this vendor's frame still
    /// agrees with the reference run's frame as well as the vendor floor asks, so a golden blessed
    /// from the reference run would pass. Without the two frames, the reference run being under its
    /// own floor stands in for the agreement. A stale golden or a look change on every device, not
    /// a vendor defect; the one verdict that does not fail `attribute`.
    Golden,
    /// This vendor's frame differs from the reference run's frame by more than the floor allows
    /// (without the two frames: the reference run is not under a floor of its own).
    Vendor,
    /// This vendor's frame agrees with the reference run's frame, but the reference run is not under
    /// a floor of its own (still inside it, or the plate has no reference floor): the change is
    /// shared by both runs and only this vendor's floor catches it, so it fails.
    Shared,
    /// The vendor run's frame, the reference run's frame and the two scores against the golden
    /// cannot come from one build: the three RGB RMSEs break the triangle inequality by more than
    /// [`TRIANGLE_SLACK_LEVELS`], or the frame sizes contradict each other. A stale `<plate>.png`, a
    /// run of another build, or the wrong directory; it fails.
    Inconsistent,
    /// The reference run did not record the plate.
    NoReference,
}

impl Attribution {
    /// The word that starts the plate's line in the command's output.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Golden => "GOLDEN",
            Self::Vendor => "VENDOR",
            Self::Shared => "SHARED",
            Self::Inconsistent => "INCONSISTENT",
            Self::NoReference => "NOREF",
        }
    }
}

/// How far, in 8-bit levels of RGB RMSE, [`attribute`] lets the three scores of a plate break the
/// triangle inequality before it calls them [`Attribution::Inconsistent`]. The `.plate` lines keep
/// three decimals of dB, which moves an RMSE by well under 0.01 levels; half a level leaves room
/// for that and nothing a real run of one build produces.
pub const TRIANGLE_SLACK_LEVELS: f64 = 0.5;

/// The RGB RMSE in 8-bit levels behind a PSNR in dB (0 for an exact match): the inverse of
/// `10 log10(255^2 / MSE)`.
fn rmse_levels(db: f64) -> f64 {
    if db.is_infinite() {
        0.0
    } else {
        255.0 * 10f64.powf(-db / 20.0)
    }
}

/// Why the three scores of a plate cannot come from one build, if they cannot: the vendor run and
/// the reference run against the golden, and the two frames against each other. RMSE over the same
/// pixels is a scaled L2 norm, so each of the three distances is at most the sum of the other two;
/// and frame size is an equivalence, so exactly one pair of different size is impossible.
fn inconsistency(vendor: &PlateScore, reference: &PlateScore, cross: &PlateScore) -> Option<String> {
    match (vendor, reference, cross) {
        (PlateScore::Db(v), PlateScore::Db(r), PlateScore::Db(c)) => {
            let (v, r, c) = (rmse_levels(*v), rmse_levels(*r), rmse_levels(*c));
            let excess = (v - r - c).max(r - v - c).max(c - v - r);
            (excess > TRIANGLE_SLACK_LEVELS).then(|| {
                format!(
                    "RGB RMSE {v:.2} (vendor, golden), {r:.2} (reference, golden) and {c:.2} (vendor, reference) \
                     levels break the triangle inequality by {excess:.2} levels"
                )
            })
        }
        _ => {
            let differ = [vendor, reference, cross]
                .iter()
                .filter(|score| matches!(score, PlateScore::SizeDiffers))
                .count();
            (differ == 1)
                .then(|| "two of the three frame pairs are the same size and the third is not".to_string())
        }
    }
}

/// One plate under its vendor floor, as [`attribute`] explains it.
#[derive(Debug, Clone, PartialEq)]
pub struct Attributed {
    /// The plate.
    pub plate: String,
    /// Whose change it is.
    pub attribution: Attribution,
    /// One line with the scores behind the verdict.
    pub line: String,
}

/// What [`attribute`] reads: a vendor run, the reference run of the same build, and their floors.
#[derive(Debug, Clone, Copy)]
pub struct AttributeInput<'a> {
    /// The vendor run: each plate's score against its golden.
    pub run: &'a BTreeMap<String, PlateScore>,
    /// The vendor's floors.
    pub floors: &'a BTreeMap<String, f64>,
    /// The reference run of the same build: each plate's score against the same golden.
    pub reference: &'a BTreeMap<String, PlateScore>,
    /// The reference vendor's floors. A plate with none is never [`Attribution::Golden`]: no check
    /// of the reference run would fail it.
    pub reference_floors: &'a BTreeMap<String, f64>,
    /// The vendor run's frame scored against the reference run's frame ([`cross_scores`]), for the
    /// plates where both frames could be read.
    pub cross: &'a BTreeMap<String, PlateScore>,
}

fn show(score: &PlateScore) -> String {
    match score {
        PlateScore::Db(db) if db.is_infinite() => "an exact match".to_string(),
        PlateScore::Db(db) => format!("{db:.2} dB"),
        PlateScore::SizeDiffers => "a size mismatch".to_string(),
    }
}

/// Explains each plate of the vendor run that is under its floor (or differs in size from its
/// golden) with a reference run of the same build. Where both frames were recorded, the three
/// scores are first held to each other ([`Attribution::Inconsistent`] when they cannot come from
/// one build); then the two frames say whether the vendor's frame still looks like the lavapipe
/// frame: [`Attribution::Vendor`] when they do not agree to the floor, [`Attribution::Golden`] when
/// they do and the reference run is under a floor of its own (a golden blessed from it would pass,
/// and the reference run's own check fails the plate), [`Attribution::Shared`] when they do and
/// it is not. Where a frame is missing, the reference run's score against its own floor decides
/// (under it: [`Attribution::Golden`]). Only [`Attribution::Golden`] excuses a plate, and only
/// because the reference run's check fails it. Plates at or above their floor, and plates the
/// vendor run did not record, are not listed.
#[must_use]
pub fn attribute(input: &AttributeInput<'_>) -> Vec<Attributed> {
    let mut out = Vec::new();
    for (plate, floor) in input.floors {
        let Some(score) = input.run.get(plate) else {
            continue;
        };
        if matches!(score, PlateScore::Db(db) if db >= floor) {
            continue;
        }
        let head = format!(
            "{plate}: {} against its golden (floor {floor:.1} dB)",
            show(score)
        );
        let Some(reference) = input.reference.get(plate) else {
            out.push(Attributed {
                plate: plate.clone(),
                attribution: Attribution::NoReference,
                line: format!("{head}; the reference run did not record it"),
            });
            continue;
        };
        // The golden takes the blame only where the reference run fails a floor of its own, so that
        // `check --vendor lavapipe` on the reference run fails the plate: an excused vendor failure
        // is then still a failure somewhere.
        let reference_bar = input.reference_floors.get(plate).copied();
        let reference_moved =
            reference_bar.is_some_and(|bar| !matches!(reference, PlateScore::Db(db) if *db >= bar));
        let reference_note = format!(
            "the reference run is at {} against the same golden{}",
            show(reference),
            match reference_bar {
                Some(bar) if reference_moved => format!(" (under its own floor {bar:.1} dB)"),
                Some(bar) => format!(" (inside its own floor {bar:.1} dB)"),
                None => " (no reference floor for this plate)".to_string(),
            }
        );
        let cross = input.cross.get(plate);
        if let Some(frames) = cross
            && let Some(why) = inconsistency(score, reference, frames)
        {
            out.push(Attributed {
                plate: plate.clone(),
                attribution: Attribution::Inconsistent,
                line: format!(
                    "{head}; {reference_note}; the two frames are at {} against each other: these scores cannot \
                     come from one build ({why}); a stale `{plate}.png`, a run of another build, or the wrong \
                     directory",
                    show(frames)
                ),
            });
            continue;
        }
        let (attribution, why) = match cross {
            Some(agree @ PlateScore::Db(db)) if db >= floor && reference_moved => (
                Attribution::Golden,
                format!(
                    "the two frames agree to {}: the golden moved (stale, or a look change on every device); \
                     blessed from the reference run it would pass this floor",
                    show(agree)
                ),
            ),
            Some(agree @ PlateScore::Db(db)) if db >= floor => (
                Attribution::Shared,
                format!(
                    "the two frames agree to {}, but the reference run is not under a floor of its own: a change \
                     both runs share that only this vendor's floor catches",
                    show(agree)
                ),
            ),
            Some(PlateScore::SizeDiffers) => (
                Attribution::Vendor,
                "the two frames differ in size: this vendor's frame diverges from the reference run's".to_string(),
            ),
            Some(agree) => (
                Attribution::Vendor,
                format!(
                    "the two frames agree to {} only: this vendor's frame diverges from the reference run's{}",
                    show(agree),
                    if reference_moved {
                        ", and the golden moved as well"
                    } else {
                        ""
                    }
                ),
            ),
            None if reference_moved => (
                Attribution::Golden,
                "the frames were not compared: the frame moved on every device (a stale golden or a look change)"
                    .to_string(),
            ),
            None => (
                Attribution::Vendor,
                "the frames were not compared: the reference run is not under a floor of its own, so nothing says \
                 the frame moved on every device; this vendor's frame diverges"
                    .to_string(),
            ),
        };
        out.push(Attributed {
            plate: plate.clone(),
            attribution,
            line: format!("{head}; {reference_note}; {why}"),
        });
    }
    out
}

/// PSNR in dB over the RGB channels of two equal-sized RGBA8 frames against a peak of 255, alpha
/// ignored: the measure `regia_test_utils::golden::psnr_rgb8` writes into the `.plate` lines (a
/// test holds the two together). `None` when the lengths differ or are not whole pixels.
fn psnr_rgb(left: &[u8], right: &[u8]) -> Option<f64> {
    if left.len() != right.len() || left.is_empty() || !left.len().is_multiple_of(4) {
        return None;
    }
    let mut sum: u64 = 0;
    for (a, b) in left.as_chunks::<4>().0.iter().zip(right.as_chunks::<4>().0) {
        for (x, y) in a[..3].iter().zip(&b[..3]) {
            let d = u64::from(x.abs_diff(*y));
            sum += d * d;
        }
    }
    if sum == 0 {
        return Some(f64::INFINITY);
    }
    let mse = sum as f64 / (left.len() / 4 * 3) as f64;
    Some(10.0 * (255.0 * 255.0 / mse).log10())
}

/// A plate name is also its file stem (`regia_test_utils::golden` writes only ascii letters,
/// digits, `-` and `_`); any other name is not looked up as a file.
fn is_plate_file_stem(plate: &str) -> bool {
    !plate.is_empty()
        && plate
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Scores the frame each of `plates` has in a vendor run's directory against the frame the
/// reference run recorded for it (`<plate>.png` in both, beside the `.plate` lines), with the PSNR
/// of the `.plate` lines. Two frames of different sizes score [`PlateScore::SizeDiffers`]. A plate
/// whose frame is missing or does not decode on either side is left out: [`attribute`] then falls
/// back to the reference run's score.
#[must_use]
pub fn cross_scores<'a>(
    run_dir: &Path,
    reference_dir: &Path,
    plates: impl IntoIterator<Item = &'a String>,
) -> BTreeMap<String, PlateScore> {
    let mut scores = BTreeMap::new();
    for plate in plates {
        if !is_plate_file_stem(plate) {
            continue;
        }
        let file = format!("{plate}.png");
        let (Ok(ours), Ok(theirs)) = (
            image::open(run_dir.join(&file)),
            image::open(reference_dir.join(&file)),
        ) else {
            continue;
        };
        let (ours, theirs) = (ours.to_rgba8(), theirs.to_rgba8());
        let score = if ours.dimensions() == theirs.dimensions() {
            psnr_rgb(ours.as_raw(), theirs.as_raw()).map_or(PlateScore::SizeDiffers, PlateScore::Db)
        } else {
            PlateScore::SizeDiffers
        };
        scores.insert(plate.clone(), score);
    }
    scores
}

/// A Markdown table of every plate of the vendor run: its floor, its score against the golden, the
/// reference run's score against the same golden, the two frames against each other, and the
/// verdict (`ok` at or above the floor, `no floor` for a plate that is not gated, else the
/// [`attribute`] verdict).
#[must_use]
pub fn attribution_table(input: &AttributeInput<'_>) -> String {
    let found = attribute(input);
    let verdicts: BTreeMap<&str, Attribution> = found
        .iter()
        .map(|entry| (entry.plate.as_str(), entry.attribution))
        .collect();
    let cell = |score: Option<&PlateScore>| match score {
        None => "-".to_string(),
        Some(PlateScore::Db(db)) if db.is_infinite() => "exact".to_string(),
        Some(PlateScore::Db(db)) => format!("{db:.2}"),
        Some(PlateScore::SizeDiffers) => "size differs".to_string(),
    };
    let mut table = String::from(
        "| plate | floor dB | vendor vs golden | reference vs golden | vendor vs reference | verdict |\n\
         |---|---|---|---|---|---|\n",
    );
    for (plate, score) in input.run {
        let floor = input.floors.get(plate);
        let verdict = match (floor, verdicts.get(plate.as_str())) {
            (None, _) => "no floor",
            (Some(_), None) => "ok",
            (Some(_), Some(Attribution::Golden)) => "golden moved",
            (Some(_), Some(Attribution::Vendor)) => "vendor diverges",
            (Some(_), Some(Attribution::Shared)) => "shared change",
            (Some(_), Some(Attribution::Inconsistent)) => "inconsistent",
            (Some(_), Some(Attribution::NoReference)) => "no reference plate",
        };
        table.push_str(
            &[
                format!("| {plate}"),
                floor.map_or_else(|| "-".to_string(), |f| format!("{f:.1}")),
                cell(Some(score)),
                cell(input.reference.get(plate)),
                cell(input.cross.get(plate)),
                format!("{verdict} |\n"),
            ]
            .join(" | "),
        );
    }
    table
}

/// What [`measure`] found: the floors, and plates worth a person's look.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Measured {
    /// Plate name to its floor.
    pub floors: BTreeMap<String, f64>,
    /// Plates left out: absent from a run, or a size mismatch in one.
    pub skipped: Vec<String>,
    /// Plates whose runs disagree, with the spread in dB.
    pub unstable: Vec<(String, f64)>,
}

/// Floors from one or more runs of the suite on one vendor: the worst run, less the spread between
/// runs and `margin_db`, capped at [`MAX_FLOOR_DB`] and rounded down to 0.1 dB. A plate must be
/// measured in every run. Scores above the cap count as the cap, so two exact-or-nearly runs do not
/// read as unstable.
#[must_use]
pub fn measure(runs: &[BTreeMap<String, PlateScore>], margin_db: f64) -> Measured {
    let mut out = Measured::default();
    let mut names: Vec<&String> = runs.iter().flat_map(BTreeMap::keys).collect();
    names.sort();
    names.dedup();
    for plate in names {
        let scores: Option<Vec<f64>> = runs
            .iter()
            .map(|run| match run.get(plate) {
                Some(PlateScore::Db(db)) => Some(db.min(MAX_FLOOR_DB + margin_db)),
                _ => None,
            })
            .collect();
        let Some(scores) = scores else {
            out.skipped.push(plate.clone());
            continue;
        };
        let worst = scores.iter().copied().fold(f64::INFINITY, f64::min);
        let best = scores.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let spread = best - worst;
        if spread > 0.05 {
            out.unstable.push((plate.clone(), spread));
        }
        let floor = ((worst - spread - margin_db).min(MAX_FLOOR_DB) * 10.0).floor() / 10.0;
        if floor > 0.0 {
            out.floors.insert(plate.clone(), floor);
        } else {
            out.skipped.push(plate.clone());
        }
    }
    out
}

/// Arguments of `cargo xtask plate-floors`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// `check`, `measure` or `attribute`.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Hold a run's plates to a vendor's floors (exit 0 pass, 1 fail, 2 vendor not gated).
    Check {
        /// The run's `REGIA_PLATE_DIR`.
        #[arg(long)]
        plates: PathBuf,
        /// `nvidia`, `amd`, `intel` or `lavapipe`.
        #[arg(long)]
        vendor: String,
        /// The floor file (default `docs/ci/plate-floors.toml` in the workspace).
        #[arg(long)]
        floors: Option<PathBuf>,
        /// A run of part of the suite: list floored plates it did not record instead of failing.
        #[arg(long)]
        partial: bool,
    },
    /// Write a vendor's floors from one or more runs.
    Measure {
        /// A run's `REGIA_PLATE_DIR`; repeat for each run.
        #[arg(long, required = true)]
        plates: Vec<PathBuf>,
        /// `nvidia`, `amd`, `intel` or `lavapipe`.
        #[arg(long)]
        vendor: String,
        /// What was measured: the card, the driver, the date and the commit.
        #[arg(long)]
        note: String,
        /// Headroom under the worst run, in dB.
        #[arg(long, default_value_t = DEFAULT_MARGIN_DB)]
        margin: f64,
        /// The floor file (default `docs/ci/plate-floors.toml` in the workspace).
        #[arg(long)]
        floors: Option<PathBuf>,
    },
    /// Explain a run's floor failures with a lavapipe run of the same build: the golden moved, this
    /// vendor diverges, a change both share, or inconsistent inputs (exit 0 only when every failure
    /// is a moved golden, 1 otherwise, 2 vendor not gated).
    Attribute {
        /// The vendor run's `REGIA_PLATE_DIR`.
        #[arg(long)]
        plates: PathBuf,
        /// The `REGIA_PLATE_DIR` of the same test binaries run on lavapipe (not the vendor run's).
        #[arg(long)]
        reference: PathBuf,
        /// `nvidia`, `amd`, `intel` or `lavapipe`.
        #[arg(long)]
        vendor: String,
        /// The floor file (default `docs/ci/plate-floors.toml` in the workspace).
        #[arg(long)]
        floors: Option<PathBuf>,
        /// Also print a Markdown table of every plate of the run with its three scores.
        #[arg(long)]
        table: bool,
    },
}

fn require_vendor(vendor: &str) -> Result<()> {
    if VENDORS.contains(&vendor) {
        Ok(())
    } else {
        bail!("unknown vendor {vendor:?} (known: {})", VENDORS.join(", "))
    }
}

/// Runs `cargo xtask plate-floors`.
///
/// # Errors
/// An unreadable floor file or run directory, or an unknown vendor.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    match &args.command {
        Command::Check {
            plates,
            vendor,
            floors,
            partial,
        } => {
            require_vendor(vendor)?;
            let path = floors.clone().unwrap_or_else(|| root.join(FLOORS_PATH));
            let text =
                std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            let file = FloorFile::parse(&text)?;
            let row = file.vendors.get(vendor).cloned().unwrap_or_default();
            if row.floors.is_empty() {
                println!("no plate floors for vendor {vendor}: not gated (measure it, then commit its row)");
                return Ok(ExitCode::from(2));
            }
            let report = check(&read_run(plates)?, &row.floors, *partial);
            for failure in &report.failures {
                println!("FAIL {failure}");
            }
            if !report.unfloored.is_empty() {
                println!(
                    "{} plate(s) have no {vendor} floor, not gated: {}",
                    report.unfloored.len(),
                    report.unfloored.join(", ")
                );
            }
            if !report.not_run.is_empty() {
                println!(
                    "{} floored plate(s) not in this partial run: {}",
                    report.not_run.len(),
                    report.not_run.join(", ")
                );
            }
            let verdict = if report.failures.is_empty() {
                "PASS"
            } else {
                "FAIL"
            };
            println!(
                "plate floors for {vendor}: {verdict} ({} passed, {} failed; measured on {})",
                report.passed,
                report.failures.len(),
                row.measured
            );
            Ok(if report.failures.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Command::Measure {
            plates,
            vendor,
            note,
            margin,
            floors,
        } => {
            require_vendor(vendor)?;
            if !margin.is_finite() || *margin < 0.0 {
                bail!("--margin must be a finite number of dB, 0 or more");
            }
            let path = floors.clone().unwrap_or_else(|| root.join(FLOORS_PATH));
            let text =
                std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            let mut file = FloorFile::parse(&text)?;
            let runs = plates
                .iter()
                .map(|dir| read_run(dir))
                .collect::<Result<Vec<_>>>()?;
            if runs.iter().any(BTreeMap::is_empty) {
                bail!("a run directory holds no plates (was REGIA_PLATE_DIR set for that run?)");
            }
            let measured = measure(&runs, *margin);
            for (plate, spread) in &measured.unstable {
                println!("unstable {plate}: runs differ by {spread:.2} dB");
            }
            for plate in &measured.skipped {
                println!("skipped {plate}: not a usable score in every run");
            }
            println!(
                "{vendor}: {} floors from {} run(s), written to {}",
                measured.floors.len(),
                runs.len(),
                path.display()
            );
            file.vendors.insert(
                vendor.clone(),
                VendorFloors {
                    measured: note.clone(),
                    floors: measured.floors,
                },
            );
            std::fs::write(&path, file.render()?).with_context(|| format!("writing {}", path.display()))?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Attribute {
            plates,
            reference,
            vendor,
            floors,
            table,
        } => {
            require_vendor(vendor)?;
            // The vendor run as its own reference scores every frame pair as an exact match and
            // would blame the golden for every vendor failure (review sprint-rev-claude-3 finding 1).
            let same_dir = |dir: &Path| std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
            if same_dir(plates) == same_dir(reference) {
                bail!(
                    "--reference {} is the vendor run itself: give the REGIA_PLATE_DIR of a {REFERENCE_VENDOR} run of \
                     the same build",
                    reference.display()
                );
            }
            let path = floors.clone().unwrap_or_else(|| root.join(FLOORS_PATH));
            let text =
                std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            let file = FloorFile::parse(&text)?;
            let row = file.vendors.get(vendor).cloned().unwrap_or_default();
            if row.floors.is_empty() {
                println!("no plate floors for vendor {vendor}: not gated (measure it, then commit its row)");
                return Ok(ExitCode::from(2));
            }
            let reference_floors = file
                .vendors
                .get(REFERENCE_VENDOR)
                .map(|row| row.floors.clone())
                .unwrap_or_default();
            let run = read_run(plates)?;
            let reference_run = read_run(reference)?;
            let cross = cross_scores(plates, reference, run.keys());
            let input = AttributeInput {
                run: &run,
                floors: &row.floors,
                reference: &reference_run,
                reference_floors: &reference_floors,
                cross: &cross,
            };
            if *table {
                print!("{}", attribution_table(&input));
            }
            let found = attribute(&input);
            for entry in &found {
                println!("{} {}", entry.attribution.label(), entry.line);
            }
            let count = |wanted: Attribution| found.iter().filter(|e| e.attribution == wanted).count();
            let golden = count(Attribution::Golden);
            println!(
                "plate floors for {vendor} against the {REFERENCE_VENDOR} run: {} plate(s) under the floor: {golden} \
                 where the golden moved, {} where {vendor} diverges, {} shared with the reference run inside its own \
                 floor, {} inconsistent, {} with no reference plate",
                found.len(),
                count(Attribution::Vendor),
                count(Attribution::Shared),
                count(Attribution::Inconsistent),
                count(Attribution::NoReference),
            );
            if golden > 0 {
                println!(
                    "the {golden} moved golden(s) fail `plate-floors check --vendor {REFERENCE_VENDOR}` on the reference run: \
                     run that check too, as this exit code does not count them"
                );
            }
            Ok(if golden == found.len() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_of(rows: &[(&str, PlateScore)]) -> BTreeMap<String, PlateScore> {
        rows.iter().map(|(n, s)| ((*n).to_string(), *s)).collect()
    }

    fn floors_of(rows: &[(&str, f64)]) -> BTreeMap<String, f64> {
        rows.iter().map(|(n, f)| ((*n).to_string(), *f)).collect()
    }

    /// The line the capture side writes is the line this side reads.
    #[test]
    fn reads_the_line_the_capture_side_writes() {
        for (psnr, expected) in [
            (Some(31.5), PlateScore::Db(31.5)),
            (Some(f64::INFINITY), PlateScore::Db(f64::INFINITY)),
            (None, PlateScore::SizeDiffers),
        ] {
            let line =
                regia_test_utils::golden::plate_line("regia_x-a", 4, 4, psnr, "crates/x/goldens/a.png");
            assert_eq!(
                parse_plate_line(&line).ok(),
                Some(("regia_x-a".to_string(), expected))
            );
        }
    }

    #[test]
    fn malformed_plate_lines_are_errors() {
        assert!(parse_plate_line("").is_err());
        assert!(parse_plate_line("a 1x1 30.0").is_err());
        assert!(parse_plate_line("a 1x1 thirty b.png").is_err());
    }

    #[test]
    fn check_fails_under_the_floor_and_passes_at_it() {
        let floors = floors_of(&[("a", 30.0), ("b", 30.0)]);
        let run = run_of(&[("a", PlateScore::Db(30.0)), ("b", PlateScore::Db(29.99))]);
        let report = check(&run, &floors, false);
        assert_eq!(report.passed, 1);
        assert_eq!(report.failures.len(), 1);
        assert!(
            report.failures[0].starts_with("b: 29.99 dB"),
            "{:?}",
            report.failures
        );
    }

    #[test]
    fn a_missing_plate_fails_unless_the_run_is_partial() {
        let floors = floors_of(&[("gone", 30.0)]);
        let strict = check(&BTreeMap::new(), &floors, false);
        assert!(
            strict.failures[0].contains("not measured"),
            "{:?}",
            strict.failures
        );
        let partial = check(
            &run_of(&[("kept", PlateScore::Db(40.0))]),
            &floors_of(&[("gone", 30.0), ("kept", 30.0)]),
            true,
        );
        assert!(partial.failures.is_empty(), "{:?}", partial.failures);
        assert_eq!(partial.not_run, ["gone"]);
    }

    /// A broken capture records nothing: `--partial` must not turn that into a pass.
    #[test]
    fn a_partial_run_with_no_floored_plate_fails() {
        let floors = floors_of(&[("a", 30.0)]);
        let empty = check(&BTreeMap::new(), &floors, true);
        assert_eq!(empty.failures.len(), 1, "{:?}", empty.failures);
        assert!(
            empty.failures[0].contains("REGIA_PLATE_DIR"),
            "{:?}",
            empty.failures
        );
        let unrelated = check(&run_of(&[("other", PlateScore::Db(40.0))]), &floors, true);
        assert_eq!(unrelated.failures.len(), 1, "{:?}", unrelated.failures);
    }

    #[test]
    fn a_size_mismatch_fails_and_a_new_golden_does_not() {
        let floors = floors_of(&[("a", 30.0)]);
        let run = run_of(&[("a", PlateScore::SizeDiffers), ("new", PlateScore::Db(1.0))]);
        let report = check(&run, &floors, false);
        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].contains("differ in size"));
        assert_eq!(report.unfloored, ["new"]);
    }

    #[test]
    fn an_exact_frame_clears_any_floor() {
        let report = check(
            &run_of(&[("a", PlateScore::Db(f64::INFINITY))]),
            &floors_of(&[("a", MAX_FLOOR_DB)]),
            false,
        );
        assert!(report.failures.is_empty());
    }

    #[test]
    fn measure_takes_the_worst_run_less_the_spread_and_margin() {
        let runs = [
            run_of(&[("steady", PlateScore::Db(33.34)), ("moves", PlateScore::Db(30.0))]),
            run_of(&[("steady", PlateScore::Db(33.34)), ("moves", PlateScore::Db(29.0))]),
        ];
        let measured = measure(&runs, 3.0);
        assert_eq!(measured.floors.get("steady"), Some(&30.3));
        assert_eq!(measured.floors.get("moves"), Some(&25.0));
        assert_eq!(measured.unstable, [("moves".to_string(), 1.0)]);
    }

    #[test]
    fn measure_caps_exact_runs_and_does_not_call_them_unstable() {
        let runs = [
            run_of(&[("exact", PlateScore::Db(f64::INFINITY))]),
            run_of(&[("exact", PlateScore::Db(70.0))]),
        ];
        let measured = measure(&runs, 3.0);
        assert_eq!(measured.floors.get("exact"), Some(&MAX_FLOOR_DB));
        assert!(measured.unstable.is_empty());
    }

    #[test]
    fn measure_skips_a_plate_absent_from_a_run_or_mismatched() {
        let runs = [
            run_of(&[
                ("only_one", PlateScore::Db(30.0)),
                ("size", PlateScore::SizeDiffers),
            ]),
            run_of(&[("size", PlateScore::Db(30.0))]),
        ];
        let measured = measure(&runs, 3.0);
        assert!(measured.floors.is_empty());
        assert_eq!(measured.skipped, ["only_one", "size"]);
    }

    #[test]
    fn a_floor_measured_from_a_run_passes_that_run() {
        let run = run_of(&[
            ("a", PlateScore::Db(12.5)),
            ("b", PlateScore::Db(f64::INFINITY)),
            ("c", PlateScore::Db(39.99)),
        ]);
        let measured = measure(std::slice::from_ref(&run), DEFAULT_MARGIN_DB);
        let report = check(&run, &measured.floors, false);
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.passed, 3);
    }

    fn valid_file() -> FloorFile {
        FloorFile {
            version: FLOORS_VERSION,
            vendors: VENDORS
                .iter()
                .map(|v| ((*v).to_string(), VendorFloors::default()))
                .collect(),
        }
    }

    #[test]
    fn the_file_survives_a_round_trip() -> Result<()> {
        let mut file = valid_file();
        file.vendors.insert(
            "nvidia".to_string(),
            VendorFloors {
                measured: "RTX 5090".to_string(),
                floors: floors_of(&[("regia_test_utils-alpha_modes", 28.5)]),
            },
        );
        let text = file.render()?;
        assert!(text.starts_with("# Per-vendor"));
        assert_eq!(FloorFile::parse(&text)?, file);
        Ok(())
    }

    #[test]
    fn bad_files_are_refused() -> Result<()> {
        let mut no_vendor = valid_file();
        no_vendor.vendors.remove("amd");
        assert!(FloorFile::parse(&no_vendor.render()?).is_err());

        let mut unknown = valid_file();
        unknown
            .vendors
            .insert("voodoo".to_string(), VendorFloors::default());
        assert!(FloorFile::parse(&unknown.render()?).is_err());

        let mut no_note = valid_file();
        no_note.vendors.insert(
            "intel".to_string(),
            VendorFloors {
                measured: String::new(),
                floors: floors_of(&[("a", 20.0)]),
            },
        );
        assert!(FloorFile::parse(&no_note.render()?).is_err());

        let mut too_high = valid_file();
        too_high.vendors.insert(
            "intel".to_string(),
            VendorFloors {
                measured: "x".to_string(),
                floors: floors_of(&[("a", MAX_FLOOR_DB + 1.0)]),
            },
        );
        assert!(FloorFile::parse(&too_high.render()?).is_err());

        let mut wrong_version = valid_file();
        wrong_version.version = FLOORS_VERSION + 1;
        assert!(FloorFile::parse(&wrong_version.render()?).is_err());
        Ok(())
    }

    /// The checked-in file parses and names all four vendors.
    #[test]
    fn the_checked_in_file_is_valid() -> Result<()> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(FLOORS_PATH);
        FloorFile::parse(&std::fs::read_to_string(path)?)?;
        Ok(())
    }

    /// Every exit of `check` through the real entry point: pass, fail, not gated.
    #[test]
    fn check_exit_codes() -> Result<()> {
        let dir = regia_test_utils::temp::unique_temp_dir("plate_floors_check");
        let plates = dir.join("plates");
        std::fs::create_dir_all(&plates)?;
        std::fs::write(plates.join("a.plate"), "a 4x4 30.000 crates/x/goldens/a.png\n")?;
        std::fs::write(plates.join("a.png"), b"not read by the check")?;

        let write_floors = |floor: f64| -> Result<PathBuf> {
            let mut file = valid_file();
            file.vendors.insert(
                "nvidia".to_string(),
                VendorFloors {
                    measured: "test".to_string(),
                    floors: floors_of(&[("a", floor)]),
                },
            );
            let path = dir.join(format!("floors-{floor}.toml"));
            std::fs::write(&path, file.render()?)?;
            Ok(path)
        };
        let code_for = |vendor: &str, floors: PathBuf| -> Result<ExitCode> {
            run(
                &dir,
                &Args {
                    command: Command::Check {
                        plates: plates.clone(),
                        vendor: vendor.to_string(),
                        floors: Some(floors),
                        partial: false,
                    },
                },
            )
        };
        let debug = |code: ExitCode| format!("{code:?}");
        assert_eq!(
            debug(code_for("nvidia", write_floors(25.0)?)?),
            debug(ExitCode::SUCCESS)
        );
        assert_eq!(
            debug(code_for("nvidia", write_floors(35.0)?)?),
            debug(ExitCode::FAILURE)
        );
        assert_eq!(
            debug(code_for("amd", write_floors(25.0)?)?),
            debug(ExitCode::from(2))
        );
        assert!(code_for("voodoo", write_floors(25.0)?).is_err());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// `measure` writes a row a later `check` passes, and leaves other vendors alone.
    #[test]
    fn measure_writes_a_row_that_check_reads() -> Result<()> {
        let dir = regia_test_utils::temp::unique_temp_dir("plate_floors_measure");
        let plates = dir.join("plates");
        std::fs::create_dir_all(&plates)?;
        std::fs::write(plates.join("a.plate"), "a 4x4 30.000 crates/x/goldens/a.png\n")?;
        let floors = dir.join("floors.toml");
        let mut start = valid_file();
        start.vendors.insert(
            "amd".to_string(),
            VendorFloors {
                measured: "kept".to_string(),
                floors: floors_of(&[("z", 20.0)]),
            },
        );
        std::fs::write(&floors, start.render()?)?;
        // The exit code is not what this test checks; the floors file it rewrites is.
        let _ = run(
            &dir,
            &Args {
                command: Command::Measure {
                    plates: vec![plates],
                    vendor: "intel".to_string(),
                    note: "Arc, test".to_string(),
                    margin: 3.0,
                    floors: Some(floors.clone()),
                },
            },
        )?;
        let file = FloorFile::parse(&std::fs::read_to_string(&floors)?)?;
        assert_eq!(file.vendors["intel"].floors["a"], 27.0);
        assert_eq!(file.vendors["intel"].measured, "Arc, test");
        assert_eq!(file.vendors["amd"].measured, "kept");
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    fn input<'a>(
        run: &'a BTreeMap<String, PlateScore>,
        floors: &'a BTreeMap<String, f64>,
        reference: &'a BTreeMap<String, PlateScore>,
        reference_floors: &'a BTreeMap<String, f64>,
        cross: &'a BTreeMap<String, PlateScore>,
    ) -> AttributeInput<'a> {
        AttributeInput {
            run,
            floors,
            reference,
            reference_floors,
            cross,
        }
    }

    fn verdicts(found: &[Attributed]) -> BTreeMap<&str, Attribution> {
        found
            .iter()
            .map(|entry| (entry.plate.as_str(), entry.attribution))
            .collect()
    }

    /// The 2026-11-11 case (ADR-5798): the RTX 5080 and an RTX 5090 failed `lookdev-cockpit` at
    /// 26.96 dB, the lavapipe run of the same commit sat at 26.98 dB against the same golden, and
    /// the card's frame agreed with the lavapipe frame to 54 dB. That is a golden that moved, not a
    /// card defect. `clouds_noon` in the same run is the other case: lavapipe still matches the
    /// golden and the card's frame is 37 dB from the lavapipe frame.
    #[test]
    fn attribute_tells_a_moved_golden_from_a_vendor_divergence() {
        let floors = floors_of(&[
            ("cockpit", 40.0),
            ("clouds_noon", 40.0),
            ("sky", 40.0),
            ("new", 40.0),
        ]);
        let reference_floors = floors_of(&[("cockpit", 40.0), ("clouds_noon", 40.0), ("sky", 40.0)]);
        let card = run_of(&[
            ("cockpit", PlateScore::Db(26.958)),
            ("clouds_noon", PlateScore::Db(36.944)),
            ("sky", PlateScore::Db(55.0)),
            ("new", PlateScore::Db(20.0)),
        ]);
        let reference = run_of(&[
            ("cockpit", PlateScore::Db(26.975)),
            ("clouds_noon", PlateScore::Db(54.651)),
            ("sky", PlateScore::Db(f64::INFINITY)),
        ]);
        let cross = run_of(&[
            ("cockpit", PlateScore::Db(54.31)),
            ("clouds_noon", PlateScore::Db(37.02)),
            ("sky", PlateScore::Db(55.0)),
        ]);
        let found = attribute(&input(&card, &floors, &reference, &reference_floors, &cross));
        let by_plate = verdicts(&found);
        assert_eq!(by_plate.get("cockpit"), Some(&Attribution::Golden));
        assert_eq!(by_plate.get("clouds_noon"), Some(&Attribution::Vendor));
        assert_eq!(by_plate.get("new"), Some(&Attribution::NoReference));
        assert_eq!(
            by_plate.get("sky"),
            None,
            "a plate at its floor is not attributed"
        );
        assert_eq!(found.len(), 3);
        let line_of = |plate: &str| {
            found
                .iter()
                .find(|entry| entry.plate == plate)
                .map(|entry| entry.line.as_str())
        };
        let cockpit = line_of("cockpit");
        assert!(
            cockpit.is_some_and(|l| l.contains("26.96") && l.contains("26.98") && l.contains("54.31")),
            "the line names the three scores: {cockpit:?}"
        );
        let clouds = line_of("clouds_noon");
        assert!(
            clouds.is_some_and(|l| l.contains("36.94") && l.contains("54.65") && l.contains("37.02 dB only")),
            "the line names the three scores: {clouds:?}"
        );
    }

    /// The verdict labels by plate (the tests that pin `SHARED` and `INCONSISTENT` go by label).
    fn labels(found: &[Attributed]) -> BTreeMap<&str, &'static str> {
        found
            .iter()
            .map(|entry| (entry.plate.as_str(), entry.attribution.label()))
            .collect()
    }

    /// The PSNR of an RGB RMSE in 8-bit levels (the inverse of the `.plate` measure).
    fn db_of_rmse(rmse: f64) -> f64 {
        20.0 * (255.0 / rmse).log10()
    }

    /// The frames say whether the vendor's frame still looks like the reference frame; the golden
    /// is to blame only when the reference run fails its own floor too: a golden that moved does
    /// not excuse a vendor frame that is also far from the reference frame, and a vendor frame that
    /// agrees with a reference frame still inside its own (lower) bar is a change both share that
    /// only this vendor's floor catches (`SHARED`, a failure; review sprint-rev-claude-3 finding 2).
    #[test]
    fn attribute_goes_by_the_two_frames_when_it_has_them() {
        let floors = floors_of(&[("both", 40.0), ("close", 40.0), ("moved", 40.0), ("sized", 40.0)]);
        let reference_floors =
            floors_of(&[("both", 40.0), ("close", 35.3), ("moved", 35.3), ("sized", 40.0)]);
        let card = run_of(&[
            ("both", PlateScore::Db(19.6)),
            ("close", PlateScore::Db(38.2)),
            ("moved", PlateScore::Db(38.2)),
            ("sized", PlateScore::SizeDiffers),
        ]);
        let reference = run_of(&[
            ("both", PlateScore::Db(20.6)),
            ("close", PlateScore::Db(38.7)),
            ("moved", PlateScore::Db(35.0)),
            ("sized", PlateScore::SizeDiffers),
        ]);
        let cross = run_of(&[
            ("both", PlateScore::Db(25.0)),
            ("close", PlateScore::Db(47.5)),
            ("moved", PlateScore::Db(47.5)),
            ("sized", PlateScore::Db(f64::INFINITY)),
        ]);
        let found = attribute(&input(&card, &floors, &reference, &reference_floors, &cross));
        let by_plate = verdicts(&found);
        assert_eq!(by_plate.get("both"), Some(&Attribution::Vendor));
        assert_eq!(
            labels(&found).get("close"),
            Some(&"SHARED"),
            "the frames agree, but the reference run is at 38.7 dB, inside its own 35.3 dB floor: \
             nothing else fails this plate, so it is not excused: {found:?}"
        );
        assert_eq!(
            by_plate.get("moved"),
            Some(&Attribution::Golden),
            "the frames agree and the reference run is under its own floor too"
        );
        assert_eq!(
            by_plate.get("sized"),
            Some(&Attribution::Golden),
            "a golden of another size that both runs disagree with alike is the golden's"
        );
        let both = found
            .iter()
            .find(|entry| entry.plate == "both")
            .map(|entry| entry.line.as_str());
        assert!(
            both.is_some_and(|l| l.contains("the golden moved as well")),
            "{both:?}"
        );

        let apart = run_of(&[("sized", PlateScore::SizeDiffers)]);
        let found = attribute(&input(&card, &floors, &reference, &reference_floors, &apart));
        assert_eq!(
            verdicts(&found).get("sized"),
            Some(&Attribution::Vendor),
            "two frames of different sizes are a vendor divergence"
        );
    }

    /// Without the frames (a run kept as `.plate` lines only) the reference run's score against
    /// the golden decides: under its own floor the frame moved on every device. A size mismatch in
    /// the vendor run is attributed like a score under the floor.
    #[test]
    fn attribute_without_frames_falls_back_to_the_reference_score() {
        let floors = floors_of(&[("a", 40.0), ("b", 40.0), ("c", 36.8), ("d", 40.0)]);
        let reference_floors = floors_of(&[("a", 40.0), ("b", 40.0), ("c", 35.3), ("d", 40.0)]);
        let card = run_of(&[
            ("a", PlateScore::Db(30.0)),
            ("b", PlateScore::SizeDiffers),
            ("c", PlateScore::Db(35.9)),
            ("d", PlateScore::Db(30.0)),
        ]);
        let reference = run_of(&[
            ("a", PlateScore::Db(39.9)),
            ("b", PlateScore::Db(60.0)),
            ("c", PlateScore::Db(38.7)),
            ("d", PlateScore::SizeDiffers),
        ]);
        let none = BTreeMap::new();
        let found = attribute(&input(&card, &floors, &reference, &reference_floors, &none));
        let by_plate = verdicts(&found);
        assert_eq!(by_plate.get("a"), Some(&Attribution::Golden));
        assert_eq!(by_plate.get("b"), Some(&Attribution::Vendor));
        assert_eq!(
            by_plate.get("c"),
            Some(&Attribution::Vendor),
            "38.7 dB is over the reference's own 35.3 dB floor"
        );
        assert_eq!(by_plate.get("d"), Some(&Attribution::Golden));
        assert!(
            found.iter().all(|entry| entry.line.contains("not compared")),
            "{found:?}"
        );
    }

    /// The `.plate` PSNR is an RGB RMSE, a scaled L2 norm, so the three scores of one build obey the
    /// triangle inequality: frames that contradict their own scores by more than half a level of
    /// RMSE (a stale `<plate>.png`, another build, the wrong directory) are `INCONSISTENT`, a
    /// failure, never a moved golden (review sprint-rev-claude-3 finding 1). Sizes are held the
    /// same way: two of the three pairs of the same size make the third the same size.
    #[test]
    fn attribute_refuses_frames_that_contradict_their_scores() {
        let plates = [
            "stale",
            "far",
            "rounding",
            "beyond_rounding",
            "size_one",
            "size_two",
        ];
        let floors: BTreeMap<String, f64> = plates.iter().map(|p| ((*p).to_string(), 40.0)).collect();
        let mut reference_floors = floors.clone();
        // The reference run's 41.3 dB is under this floor, so a consistent plate is `GOLDEN`.
        reference_floors.insert("rounding".to_string(), 45.0);
        let card = run_of(&[
            // 27 dB from the golden, yet its frame is the reference frame and that is the golden.
            ("stale", PlateScore::Db(27.0)),
            // Both runs near the golden, their frames far apart.
            ("far", PlateScore::Db(39.0)),
            // RMSE 3.0 against 2.2 + 0.5: 0.3 levels over the sum, within rounding.
            ("rounding", PlateScore::Db(db_of_rmse(3.0))),
            // RMSE 3.0 against 1.9 + 0.5: 0.6 levels over the sum.
            ("beyond_rounding", PlateScore::Db(db_of_rmse(3.0))),
            ("size_one", PlateScore::Db(30.0)),
            ("size_two", PlateScore::SizeDiffers),
        ]);
        let reference = run_of(&[
            ("stale", PlateScore::Db(f64::INFINITY)),
            ("far", PlateScore::Db(45.0)),
            ("rounding", PlateScore::Db(db_of_rmse(2.2))),
            ("beyond_rounding", PlateScore::Db(db_of_rmse(1.9))),
            ("size_one", PlateScore::Db(30.0)),
            ("size_two", PlateScore::Db(f64::INFINITY)),
        ]);
        let cross = run_of(&[
            ("stale", PlateScore::Db(f64::INFINITY)),
            ("far", PlateScore::Db(25.0)),
            ("rounding", PlateScore::Db(db_of_rmse(0.5))),
            ("beyond_rounding", PlateScore::Db(db_of_rmse(0.5))),
            ("size_one", PlateScore::SizeDiffers),
            ("size_two", PlateScore::Db(f64::INFINITY)),
        ]);
        let found = attribute(&input(&card, &floors, &reference, &reference_floors, &cross));
        let by_plate = labels(&found);
        for plate in ["stale", "far", "beyond_rounding", "size_one", "size_two"] {
            assert_eq!(by_plate.get(plate), Some(&"INCONSISTENT"), "{plate}: {found:?}");
        }
        assert_eq!(
            by_plate.get("rounding"),
            Some(&"GOLDEN"),
            "0.3 levels is rounding; the reference is under its own floor: {found:?}"
        );
        let stale = found.iter().find(|entry| entry.plate == "stale");
        assert!(
            stale.is_some_and(|entry| entry.line.contains("cannot come from one build")),
            "{stale:?}"
        );
    }

    /// The golden is blamed only when the reference run fails a floor of its own, so that
    /// `plate-floors check --vendor lavapipe` on the reference run fails the plate: with no
    /// reference floor for the plate nothing else would, and the vendor's floor stays the net. The
    /// review's case: a change moves the plate to 36 dB on every device, under the vendor's 40 dB
    /// floor and over the reference's 35.3 dB one.
    #[test]
    fn attribute_blames_the_golden_only_when_the_reference_fails_its_own_floor() {
        let floors = floors_of(&[
            ("everywhere", 40.0),
            ("unfloored", 40.0),
            ("unfloored_no_frames", 40.0),
        ]);
        let reference_floors = floors_of(&[("everywhere", 35.3)]);
        let card = run_of(&[
            ("everywhere", PlateScore::Db(36.0)),
            ("unfloored", PlateScore::Db(30.0)),
            ("unfloored_no_frames", PlateScore::Db(30.0)),
        ]);
        let reference = run_of(&[
            ("everywhere", PlateScore::Db(36.1)),
            ("unfloored", PlateScore::Db(30.2)),
            ("unfloored_no_frames", PlateScore::Db(30.2)),
        ]);
        let cross = run_of(&[
            ("everywhere", PlateScore::Db(55.0)),
            ("unfloored", PlateScore::Db(55.0)),
        ]);
        let found = attribute(&input(&card, &floors, &reference, &reference_floors, &cross));
        let by_plate = labels(&found);
        assert_eq!(by_plate.get("everywhere"), Some(&"SHARED"), "{found:?}");
        assert_eq!(
            by_plate.get("unfloored"),
            Some(&"SHARED"),
            "no reference floor: nothing else gates the plate: {found:?}"
        );
        assert_eq!(
            by_plate.get("unfloored_no_frames"),
            Some(&"VENDOR"),
            "no frames and no reference floor: {found:?}"
        );
        let everywhere = found.iter().find(|entry| entry.plate == "everywhere");
        assert!(
            everywhere.is_some_and(|entry| entry.line.contains("only this vendor's floor catches")),
            "{everywhere:?}"
        );
    }

    fn write_frame(path: &Path, width: u32, height: u32, value: u8) -> Result<Vec<u8>> {
        let pixels: Vec<u8> = (0..width * height)
            .flat_map(|i| [value, value.wrapping_add((i % 7) as u8), 40, 255])
            .collect();
        image::save_buffer(path, &pixels, width, height, image::ColorType::Rgba8)?;
        Ok(pixels)
    }

    /// `cross_scores` scores the two recorded frames of a plate with the PSNR the capture side
    /// writes into the `.plate` lines, and leaves out what it cannot read.
    #[test]
    fn cross_scores_compares_the_recorded_frames() -> Result<()> {
        let dir = regia_test_utils::temp::unique_temp_dir("plate_floors_cross");
        let card = dir.join("card");
        let reference = dir.join("reference");
        std::fs::create_dir_all(&card)?;
        std::fs::create_dir_all(&reference)?;
        let same = write_frame(&card.join("same.png"), 8, 4, 100)?;
        write_frame(&reference.join("same.png"), 8, 4, 100)?;
        let ours = write_frame(&card.join("moved.png"), 8, 4, 100)?;
        let theirs = write_frame(&reference.join("moved.png"), 8, 4, 109)?;
        write_frame(&card.join("sized.png"), 8, 4, 100)?;
        write_frame(&reference.join("sized.png"), 4, 8, 100)?;
        write_frame(&card.join("alone.png"), 8, 4, 100)?;
        std::fs::write(card.join("broken.png"), b"not a png")?;
        write_frame(&reference.join("broken.png"), 8, 4, 100)?;
        write_frame(&dir.join("outside.png"), 8, 4, 100)?;

        let names: Vec<String> = ["same", "moved", "sized", "alone", "broken", "../outside", ""]
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        let scores = cross_scores(&card, &reference, &names);
        assert_eq!(scores.get("same"), Some(&PlateScore::Db(f64::INFINITY)));
        assert_eq!(scores.get("sized"), Some(&PlateScore::SizeDiffers));
        let expected = regia_test_utils::golden::psnr_rgb8(&ours, &theirs);
        assert!(
            expected.is_some_and(f64::is_finite),
            "the two frames differ: {expected:?}"
        );
        assert_eq!(scores.get("moved").copied(), expected.map(PlateScore::Db));
        assert_eq!(
            regia_test_utils::golden::psnr_rgb8(&same, &same),
            Some(f64::INFINITY)
        );
        assert_eq!(
            scores.len(),
            3,
            "a missing frame, an unreadable frame and a name that is not a file stem are left out: {scores:?}"
        );
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// The table lists every plate of the vendor run, floored or not, with the three scores.
    #[test]
    fn the_table_lists_every_plate_with_its_three_scores() {
        let floors = floors_of(&[("cockpit", 40.0), ("sky", 40.0), ("absent", 40.0)]);
        let card = run_of(&[
            ("cockpit", PlateScore::Db(26.958)),
            ("sky", PlateScore::Db(f64::INFINITY)),
            ("unfloored", PlateScore::Db(31.0)),
        ]);
        let reference = run_of(&[("cockpit", PlateScore::Db(26.975)), ("sky", PlateScore::Db(60.5))]);
        let cross = run_of(&[("cockpit", PlateScore::Db(54.31)), ("sky", PlateScore::Db(60.5))]);
        let table = attribution_table(&input(&card, &floors, &reference, &floors, &cross));
        let rows: Vec<&str> = table.lines().collect();
        assert_eq!(rows.len(), 5, "{table}");
        assert!(rows[0].starts_with("| plate | floor dB |"), "{table}");
        assert_eq!(
            rows[2],
            "| cockpit | 40.0 | 26.96 | 26.98 | 54.31 | golden moved |"
        );
        assert_eq!(rows[3], "| sky | 40.0 | exact | 60.50 | 60.50 | ok |");
        assert_eq!(rows[4], "| unfloored | - | 31.00 | - | - | no floor |");
    }

    /// `attribute` through the real entry point: exit 0 when no plate is the vendor's own, 1 when
    /// one is, 2 when the vendor has no floors; with and without the frames; the floors file is
    /// only read.
    #[test]
    fn attribute_exit_codes() -> Result<()> {
        let dir = regia_test_utils::temp::unique_temp_dir("plate_floors_attribute");
        let card = dir.join("card");
        let reference = dir.join("lavapipe");
        std::fs::create_dir_all(&card)?;
        std::fs::create_dir_all(&reference)?;
        std::fs::write(card.join("a.plate"), "a 4x4 27.000 crates/x/goldens/a.png\n")?;
        let mut file = valid_file();
        file.vendors.insert(
            "nvidia".to_string(),
            VendorFloors {
                measured: "test".to_string(),
                floors: floors_of(&[("a", 40.0)]),
            },
        );
        file.vendors.insert(
            REFERENCE_VENDOR.to_string(),
            VendorFloors {
                measured: "test".to_string(),
                floors: floors_of(&[("a", 40.0)]),
            },
        );
        let floors = dir.join("floors.toml");
        let text = file.render()?;
        std::fs::write(&floors, &text)?;
        let code_with = |vendor: &str, reference: &Path| -> Result<ExitCode> {
            run(
                &dir,
                &Args {
                    command: Command::Attribute {
                        plates: card.clone(),
                        reference: reference.to_path_buf(),
                        vendor: vendor.to_string(),
                        floors: Some(floors.clone()),
                        table: true,
                    },
                },
            )
        };
        let code_for = |vendor: &str| code_with(vendor, &reference);
        let debug = |code: ExitCode| format!("{code:?}");

        // No frames: the reference run's score decides.
        std::fs::write(reference.join("a.plate"), "a 4x4 27.010 crates/x/goldens/a.png\n")?;
        assert_eq!(debug(code_for("nvidia")?), debug(ExitCode::SUCCESS));
        std::fs::write(reference.join("a.plate"), "a 4x4 inf crates/x/goldens/a.png\n")?;
        assert_eq!(debug(code_for("nvidia")?), debug(ExitCode::FAILURE));

        // With frames, they decide where they agree with the scores: the same frame on both sides
        // with both runs under the floor is a moved golden.
        write_frame(&card.join("a.png"), 4, 4, 100)?;
        write_frame(&reference.join("a.png"), 4, 4, 100)?;
        std::fs::write(reference.join("a.plate"), "a 4x4 27.010 crates/x/goldens/a.png\n")?;
        assert_eq!(debug(code_for("nvidia")?), debug(ExitCode::SUCCESS));
        // The same frame on both sides while the reference run matches the golden exactly and the
        // vendor run is 27 dB from it cannot come from one build (a stale frame, the wrong
        // directory): it fails, it never excuses the vendor (review finding 1).
        std::fs::write(reference.join("a.plate"), "a 4x4 inf crates/x/goldens/a.png\n")?;
        assert_eq!(debug(code_for("nvidia")?), debug(ExitCode::FAILURE));
        // Two frames far apart, consistent with the scores, are the vendor's.
        write_frame(&reference.join("a.png"), 4, 4, 160)?;
        std::fs::write(card.join("a.plate"), "a 4x4 14.000 crates/x/goldens/a.png\n")?;
        std::fs::write(reference.join("a.plate"), "a 4x4 27.010 crates/x/goldens/a.png\n")?;
        assert_eq!(debug(code_for("nvidia")?), debug(ExitCode::FAILURE));

        // The vendor run given as its own reference is refused, however the path is spelled.
        assert!(code_with("nvidia", &card).is_err());
        assert!(code_with("nvidia", &card.join(".")).is_err());

        // A plate the reference run never recorded cannot be excused.
        std::fs::remove_file(reference.join("a.plate"))?;
        assert_eq!(debug(code_for("nvidia")?), debug(ExitCode::FAILURE));

        assert_eq!(debug(code_for("amd")?), debug(ExitCode::from(2)));
        assert!(code_for("voodoo").is_err());
        assert_eq!(
            std::fs::read_to_string(&floors)?,
            text,
            "attribute never writes the floors"
        );
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
