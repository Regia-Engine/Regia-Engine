// SPDX-License-Identifier: MPL-2.0
//! `regia contact-sheet`: a labeled JPEG grid and a luma CSV (ADR-8036).
//!
//! One of `--frames`, `--dirs` or `--pairs` is required. Exit 0 writes the JPEG and the CSV
//! beside it. Exit 2 is a usage error, a missing file or an unsupported image. The codes are
//! `contact_sheet_usage`, `contact_sheet_io` and `contact_sheet_format`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{ArgGroup, Args};

use regia_cli::contact_sheet::{
    self, ContactSheetError, DEFAULT_MAX_WIDTH, DEFAULT_QUALITY, SheetOptions, assign_labels,
    effective_columns, load_dir, load_named, load_pair_dirs, parse_roi,
};

/// `regia contact-sheet` arguments.
#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("source")
        .required(true)
        .multiple(false)
        .args(["frames", "dirs", "pairs"])
))]
pub struct ContactSheetArgs {
    /// Image files, in the order they appear on the sheet.
    #[arg(long, num_args = 1..)]
    pub frames: Vec<PathBuf>,
    /// Directories of images. Each directory is sorted by file name. Other files are skipped.
    #[arg(long, num_args = 1..)]
    pub dirs: Vec<PathBuf>,
    /// Two directories paired by file name: `BEFORE AFTER`.
    #[arg(long, num_args = 2, value_names = ["BEFORE", "AFTER"])]
    pub pairs: Vec<PathBuf>,
    /// JPEG to write. The CSV is the same path with a `.csv` suffix.
    #[arg(long)]
    pub out: PathBuf,
    /// Columns. A pair sheet is widened to the next even count (default 4).
    #[arg(long, default_value_t = 4)]
    pub cols: i64,
    /// Scale the sheet down with nearest-neighbor when it is wider than this (default 3840).
    #[arg(long, default_value_t = DEFAULT_MAX_WIDTH)]
    pub max_width: u32,
    /// JPEG quality, 1 to 100 (default 90).
    #[arg(long, default_value_t = DEFAULT_QUALITY)]
    pub quality: u8,
    /// Label text. One label marks every cell. With pairs, one label per pair is shared by
    /// both sides. Otherwise pass one label per cell.
    #[arg(long = "label")]
    pub labels: Vec<String>,
    /// Metric window as fractions `x,y,w,h`. The drawn cell stays the whole image.
    #[arg(long)]
    pub roi: Option<String>,
    /// Leave luma out of the caption and the CSV.
    #[arg(long, default_value_t = false)]
    pub no_metrics: bool,
}

/// Build the sheet and write it.
///
/// # Errors
///
/// The arguments cannot build a sheet, a file cannot be read or written, or an image is not
/// a PNG, JPEG, PPM or PGM this tool reads.
pub fn build(args: &ContactSheetArgs) -> Result<PathBuf, ContactSheetError> {
    let mut images = if !args.frames.is_empty() {
        args.frames
            .iter()
            .map(|path| load_named(path, None, None, None))
            .collect::<Result<Vec<_>, _>>()?
    } else if !args.dirs.is_empty() {
        let mut loaded = Vec::new();
        for dir in &args.dirs {
            loaded.extend(load_dir(dir)?);
        }
        loaded
    } else if args.pairs.len() == 2 {
        load_pair_dirs(&args.pairs[0], &args.pairs[1])?
    } else {
        return Err(ContactSheetError::Usage(
            "pass --frames, --dirs, or --pairs BEFORE AFTER".to_owned(),
        ));
    };
    assign_labels(&mut images, &args.labels)?;
    let cols_i32 = i32::try_from(args.cols).unwrap_or(i32::MAX);
    let cols = effective_columns(cols_i32, !args.pairs.is_empty())?;
    let roi = args.roi.as_deref().map(parse_roi).transpose()?;
    let output = contact_sheet::build_sheet(
        &images,
        &SheetOptions {
            cols,
            max_width: args.max_width,
            quality: args.quality,
            roi,
            metrics: !args.no_metrics,
        },
    )?;
    let csv = contact_sheet::write_sheet_files(&args.out, &output)?;
    println!("{}", csv.display());
    Ok(args.out.clone())
}

/// Run `regia contact-sheet`. Exit 0 on success, 2 on a handled error.
pub fn run(args: &ContactSheetArgs) -> ExitCode {
    match build(args) {
        Ok(path) => {
            println!("{}", path.display());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(dir: &std::path::Path) -> ContactSheetArgs {
        ContactSheetArgs {
            frames: vec![dir.join("no-such.png")],
            dirs: Vec::new(),
            pairs: Vec::new(),
            out: dir.join("sheet.jpg"),
            cols: 4,
            max_width: DEFAULT_MAX_WIDTH,
            quality: DEFAULT_QUALITY,
            labels: Vec::new(),
            roi: None,
            no_metrics: false,
        }
    }

    #[test]
    fn a_missing_frame_is_exit_2() {
        let dir = regia_test_utils::unique_temp_dir("contact-cli-missing");
        std::fs::create_dir_all(&dir).expect("dir");
        let err = build(&args(&dir)).expect_err("missing");
        let text = err.to_string();
        assert!(text.contains("cannot read image"), "{text}");
        assert!(text.starts_with("contact_sheet_io"), "{text}");
        assert_eq!(run(&args(&dir)), ExitCode::from(2));
    }

    #[test]
    fn a_ppm_frame_writes_a_jpeg_and_a_csv() {
        let dir = regia_test_utils::unique_temp_dir("contact-cli-ppm");
        std::fs::create_dir_all(&dir).expect("dir");
        let frame = dir.join("shot.ppm");
        std::fs::write(
            &frame,
            b"P6\n2 2\n255\n\xc8\x1e\x50\xc8\x1e\x50\xc8\x1e\x50\xc8\x1e\x50",
        )
        .expect("ppm");
        let mut args = args(&dir);
        args.frames = vec![frame];
        args.out = dir.join("sheet.jpg");
        let written = build(&args).expect("sheet");
        assert!(std::fs::read(&written).expect("jpeg").starts_with(&[0xFF, 0xD8]));
        let csv = std::fs::read_to_string(dir.join("sheet.csv")).expect("csv");
        assert!(csv.contains("mean_luma"), "{csv}");
        assert!(csv.contains("shot"), "{csv}");
    }

    #[test]
    fn quality_zero_and_bad_roi_are_usage() {
        let dir = regia_test_utils::unique_temp_dir("contact-cli-usage");
        std::fs::create_dir_all(&dir).expect("dir");
        let frame = dir.join("shot.ppm");
        std::fs::write(&frame, b"P6\n1 1\n255\n\x01\x02\x03").expect("ppm");
        let mut args = args(&dir);
        args.frames = vec![frame];
        args.quality = 0;
        let err = build(&args).expect_err("quality");
        assert!(err.to_string().starts_with("contact_sheet_usage"), "{err}");
        args.quality = 90;
        args.roi = Some("2,0,1,1".to_owned());
        let err = build(&args).expect_err("roi");
        assert!(err.to_string().contains("roi"), "{err}");
    }
}
