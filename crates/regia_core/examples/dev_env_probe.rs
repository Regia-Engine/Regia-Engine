// SPDX-License-Identifier: MPL-2.0
//! Release artifact probe: without devtools, neither the hook nor its key is retained.
fn main() {
    if regia_core::log::devenv::flag("REGIA_DEVENV_RELEASE_SENTINEL_8066") {
        println!("developer hook executed");
    } else {
        println!("developer hook absent");
    }
}

#[test]
#[cfg(not(feature = "devtools"))]
fn dev_env_hook_absent_in_release_build() {
    use regia_core::log::devenv;
    assert!(!std::hint::black_box(devenv::ENABLED));
    let (name, _) = std::env::vars_os().next().expect("test environment");
    let name = name.to_str().expect("environment key");
    assert_eq!(devenv::var(name), Err(std::env::VarError::NotPresent));
    assert!(devenv::var_os(name).is_none());
    assert!(!devenv::is_set(name));
    assert!(!devenv::flag(name));
}
