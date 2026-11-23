// SPDX-License-Identifier: MPL-2.0
//! The fuzz-target registry under libFuzzer (ADR-2723): one binary, the target picked by the `REGIA_FUZZ_TARGET`
//! environment variable, so 200 targets cost one ASan build. `cargo xtask fuzz run` sets the variable and the flags.
//! The verdict is `one_input::check_one`, which `regia_agent::fuzz_targets::run` also gives; any finding ends the process,
//! so libFuzzer writes the input as a crash.
#![no_main]

#[path = "../../xtask/src/fuzz/extra_targets.rs"]
mod extra_targets;
#[path = "../../xtask/src/fuzz/one_input.rs"]
mod one_input;

use std::sync::OnceLock;

use libfuzzer_sys::{fuzz_mutator, fuzz_target, fuzzer_mutate};
use regia_agent::fuzz_targets::{self, FuzzInput, FuzzTarget};
use regia_core::fuzz::{FuzzRng, mutate_bytes, mutate_text, mutate_text_values};

/// The target this process fuzzes, resolved once. A wrong name ends the process with the reason.
fn selected() -> &'static FuzzTarget {
    static TARGET: OnceLock<FuzzTarget> = OnceLock::new();
    TARGET.get_or_init(|| {
        let name = std::env::var("REGIA_FUZZ_TARGET").unwrap_or_default();
        let found = fuzz_targets::target(&name)
            .copied()
            .or_else(|| extra_targets::targets().into_iter().find(|t| t.name == name));
        let Some(target) = found else {
            eprintln!("REGIA_FUZZ_TARGET={name:?} names no target: see `cargo xtask fuzz list`");
            std::process::exit(2);
        };
        // libfuzzer-sys's hook aborts at the panic site, even inside a `catch_unwind` a loader uses on purpose. A finding
        // should mean what it means in `fuzz_targets::run`: a panic that leaves the loader. Print, unwind, let
        // `check_one` decide, and abort there.
        std::panic::set_hook(Box::new(|info| eprintln!("{info}")));
        target
    })
}

fuzz_target!(|data: &[u8]| {
    let target = selected();
    if let Err(finding) = one_input::check_one(target, data) {
        eprintln!("regia-fuzz finding in {}: {finding}", target.name);
        std::process::abort();
    }
});

// Mixes the registry's own mutators in, one mutation in four: libFuzzer's byte edits break RON and JSON at the first
// token, while `mutate_text_values` keeps the text well formed and corrupts the values (spec 97 § 2.1), so coverage
// feedback also sees validation and consumers. Seeded by libFuzzer, so a run still repeats.
fuzz_mutator!(|data: &mut [u8], size: usize, max_size: usize, seed: u32| {
    let mut rng = FuzzRng::new(u64::from(seed) ^ 0x5245_4749_4146_555A);
    let size = size.min(data.len());
    if rng.below(4) != 0 {
        return fuzzer_mutate(data, size, max_size);
    }
    let mutated = match selected().input {
        FuzzInput::Text { .. } => std::str::from_utf8(&data[..size]).ok().map(|text| {
            if rng.below(2) == 0 {
                mutate_text(text, &mut rng).into_bytes()
            } else {
                mutate_text_values(text, &mut rng).into_bytes()
            }
        }),
        FuzzInput::Bytes { .. } => Some(mutate_bytes(&data[..size], &mut rng)),
    };
    match mutated {
        Some(bytes) if bytes.len() <= max_size.min(data.len()) => {
            data[..bytes.len()].copy_from_slice(&bytes);
            bytes.len()
        }
        _ => fuzzer_mutate(data, size, max_size),
    }
});
