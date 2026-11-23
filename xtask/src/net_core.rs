// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask check-net-core` (spec 120 § 3.2 and § 15.5, ADR-8501): the gate check that keeps the
//! ECS-free core of `regia_net` honest. Three parts, all run by `check-net-core` and by `preflight`:
//!
//! 1. The no-ECS dependency assertion over `cargo tree -p regia_net --no-default-features`
//!    ([`crate::layers::check_net_core_tree`]; `check-layers` runs it too).
//! 2. `cargo check -p regia_net --no-default-features` of the library and the `net_core_headless`
//!    example (a headless server, two clients, the tick driver and core net messages).
//! 3. The core's unit tests and the example's integration test without the feature: a two-client
//!    loopback session.

/// The cargo runs of the check, with what each proves (for the failure line).
pub const COMMANDS: &[(&[&str], &str)] = &[
    (
        &[
            "check",
            "--locked",
            "-p",
            "regia_net",
            "--no-default-features",
            "--lib",
            "--example",
            "net_core_headless",
        ],
        "the regia_net core and its headless example build without the ecs feature",
    ),
    (
        &[
            "test",
            "--locked",
            "-p",
            "regia_net",
            "--no-default-features",
            "--lib",
            "--test",
            "net_core_headless",
        ],
        "the core's unit tests and the headless example's two-client loopback session run without the ecs feature",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_builds_regia_net_without_default_features() {
        for (args, what) in COMMANDS {
            assert!(args.contains(&"--no-default-features"), "{what}: {args:?}");
            assert!(
                args.windows(2).any(|w| w == ["-p", "regia_net"]),
                "{what}: {args:?}"
            );
            assert!(args.contains(&"--locked"), "{what}: {args:?}");
            assert!(
                !args.contains(&"--features"),
                "{what}: no feature may come back on"
            );
        }
    }

    #[test]
    fn the_check_builds_the_example_and_runs_its_test() {
        let example = COMMANDS
            .iter()
            .any(|(a, _)| a[0] == "check" && a.windows(2).any(|w| w == ["--example", "net_core_headless"]));
        let test = COMMANDS
            .iter()
            .any(|(a, _)| a[0] == "test" && a.windows(2).any(|w| w == ["--test", "net_core_headless"]));
        assert!(example && test, "{COMMANDS:?}");
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .join("crates/regia_net");
        assert!(root.join("examples/net_core_headless.rs").is_file());
        assert!(root.join("tests/net_core_headless.rs").is_file());
    }
}
