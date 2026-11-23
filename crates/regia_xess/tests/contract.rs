// SPDX-License-Identifier: MPL-2.0
//! CPU acceptance checks: no SDK, device or golden required.
use regia_xess::{Error, Quality, Version};

#[test]
fn quality_modes_have_distinct_sdk_values() {
    assert_eq!(Quality::Native.sdk_value(), 106);
    assert_eq!(Quality::Quality.sdk_value(), 103);
    assert_eq!(Quality::Balanced.sdk_value(), 102);
    assert_eq!(Quality::Performance.sdk_value(), 101);
    assert_eq!(Quality::UltraPerformance.sdk_value(), 100);
}

#[test]
fn errors_have_protocol_safe_codes() {
    for error in [
        Error::NotLinked,
        Error::NotVulkan,
        Error::InvalidInput,
        Error::RuntimeNotFound,
        Error::UnsupportedVersion,
        Error::DeviceNotPrepared,
        Error::Sdk {
            call: "test",
            result: -2,
        },
        Error::Backend("test".into()),
    ] {
        assert!(
            error
                .code()
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        );
    }
    assert_eq!(
        Error::Sdk {
            call: "test",
            result: -1
        }
        .code(),
        "unsupported_device"
    );
    assert_eq!(
        Error::Sdk {
            call: "test",
            result: -2
        }
        .code(),
        "needs_newer_driver"
    );
}

#[test]
fn version_check_rejects_unknown_abi_and_development_versions() {
    assert!(
        Version {
            major: 2,
            minor: 1,
            patch: 0
        }
        .supported()
    );
    assert!(
        !Version {
            major: 99,
            minor: 0,
            patch: 0
        }
        .supported()
    );
    assert!(
        !Version {
            major: 2,
            minor: 1,
            patch: 90
        }
        .supported()
    );
}

#[test]
fn unlinked_runtime_fails_before_loading_a_library() {
    if !regia_xess::LINKED {
        assert_eq!(
            regia_xess::Runtime::load(std::path::Path::new("missing.dll")).err(),
            Some(Error::NotLinked)
        );
    }
}
