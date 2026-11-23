// SPDX-License-Identifier: MPL-2.0
//! Cook target platform tags.

/// Return the host platform string used for dev cooks (`linux`, `windows`, `android`, `macos`;
/// any other OS keeps its `std::env::consts::OS` name).
///
/// The runtime reads `.regia/cooked/<platform>` with `regia_asset::current_platform`, so the
/// two must agree on every target, including ones the engine does not ship.
pub fn host_platform() -> &'static str {
    std::env::consts::OS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dev cook writes where the runtime reads, on whatever OS the tests run.
    #[test]
    fn host_platform_matches_the_runtime_directory() {
        assert_eq!(host_platform(), regia_asset::current_platform());
    }
}
