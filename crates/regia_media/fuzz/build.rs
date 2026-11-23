// SPDX-License-Identifier: MPL-2.0
//! Gives AddressSanitizer back its `pthread_create` on Linux; `fuzz/build.rs` explains why (ADR-2723): `crash-handler`,
//! through `regia_platform`, hides ASan's interceptor, and libFuzzer aborts at its start.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("TARGET").is_ok_and(|target| target.contains("linux")) {
        println!("cargo:rustc-link-arg-bins=-Wl,--defsym=pthread_create=__interceptor_pthread_create");
    }
}
