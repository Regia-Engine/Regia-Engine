// SPDX-License-Identifier: MPL-2.0
//! `regia assets contract-check` (ADR-8450, `book/agents/delivery-contract.md`).
//!
//! Judges one or more glTF files against a `.contract.ron` and writes `report.md` and `report.json`.
//! One row per failure names the asset, the node, socket or clip, and what was expected against
//! what was found. Warnings do not fail the command.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Args;
use regia_asset::delivery_contract::{DeliveryContract, DeliveryReport, check_delivery, write_vendor_report};

/// `regia assets contract-check` arguments.
#[derive(Debug, Args)]
pub struct AssetsContractCheckArgs {
    /// The `.contract.ron` for this asset class.
    #[arg(long, value_name = "FILE")]
    pub contract: PathBuf,
    /// Directory that receives `report.md` and `report.json`.
    #[arg(long, value_name = "DIR")]
    pub report: PathBuf,
    /// A glTF or GLB file to judge. Repeat for each asset.
    #[arg(long = "asset", value_name = "FILE", required = true)]
    pub assets: Vec<PathBuf>,
}

/// Judges each `--asset` and writes the vendor report.
///
/// Exits with an error when any finding is an error. A report of only warnings still writes both
/// files and returns success. A missing file, a broken glTF or a contract the loader refuses
/// fails before a report is written.
///
/// # Errors
///
/// The contract or an asset cannot be read, the contract is invalid, or an asset fails a check.
pub fn run(args: &AssetsContractCheckArgs) -> Result<()> {
    let text = std::fs::read_to_string(&args.contract)
        .with_context(|| format!("read {}", args.contract.display()))?;
    let contract =
        DeliveryContract::from_ron_str(&text).map_err(|err| anyhow::anyhow!("{}: {err}", err.code()))?;
    let mut findings = Vec::new();
    for asset in &args.assets {
        let name = asset
            .file_name()
            .map(|file| file.to_string_lossy().into_owned())
            .unwrap_or_else(|| "asset".to_string());
        let observation = regia_asset_pipeline::contract_observe::observe_gltf(asset)
            .with_context(|| format!("observe {}", asset.display()))?;
        let report = check_delivery(&contract, &observation)
            .map_err(|err| anyhow::anyhow!("{}: {err}", err.code()))?;
        for mut finding in report.findings {
            if finding.asset.is_empty() {
                finding.asset = name.clone();
            }
            findings.push(finding);
        }
    }
    let error_count = findings
        .iter()
        .filter(|finding| finding.severity == regia_asset::delivery_contract::FindingSeverity::Error)
        .count();
    let merged = DeliveryReport { findings };
    write_vendor_report(&args.report, "", &merged)
        .with_context(|| format!("write {}", args.report.display()))?;
    if error_count > 0 {
        bail!("{error_count} contract error(s); see {}", args.report.display());
    }
    Ok(())
}
