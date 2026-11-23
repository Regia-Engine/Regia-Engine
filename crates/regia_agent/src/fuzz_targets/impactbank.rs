// SPDX-License-Identifier: MPL-2.0
//! The `impactbank` target (ADR-3270): a contact-sound bank (`.impactbank.ron` v1). The loader
//! parses and validates it as `audio.impacts_set` does, builds the bank, resolves a few material
//! pairs and evaluates the curves, then writes it back.

use regia_audio::{IMPACT_BANK_FORMAT_VERSION, ImpactBank, ImpactBankDef};

use super::FuzzTarget;
use super::formats::text;

fn load(s: &str) -> Result<String, String> {
    let def = ImpactBankDef::from_ron(s).map_err(|e| e.to_string())?;
    let bank = ImpactBank::new(def.clone());
    for (a, b) in [("wood", "concrete"), ("metal", "concrete"), ("", "")] {
        let _ = bank.resolve(a, b);
    }
    let _ = bank.impact_db(4.0, 3.0);
    let _ = bank.contact_loop(0.5, 2.0, 50.0);
    def.to_ron().map_err(|e| e.to_string())
}

pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![text(
        "impactbank",
        Some("impactbank"),
        IMPACT_BANK_FORMAT_VERSION,
        true,
        || include_str!("fixtures/impactbank.ron").into(),
        load,
    )]
}
