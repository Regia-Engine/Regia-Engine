// SPDX-License-Identifier: MPL-2.0
//! Gives AddressSanitizer back its `pthread_create` on Linux (ADR-2723). `crash-handler` (through `regia_platform`) defines a
//! strong `pthread_create` that installs an alternate signal stack and calls the libc one, so ASan's interceptor never
//! sees a thread being created; its `pthread_detach` and `pthread_join` interceptors still run and abort on the unknown
//! thread, at libFuzzer's start. Only this binary changes: the symbol it exports (`libstdc++.so` resolves it dynamically, so a
//! link-time `--wrap` does not reach that caller) becomes the interceptor, which calls libc. The crate's own
//! `sanitizer_compat` cfg would fix it from the other side, but needs `RUSTFLAGS`, which rebuilds the whole crate graph.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("TARGET").is_ok_and(|target| target.contains("linux")) {
        println!("cargo:rustc-link-arg-bins=-Wl,--defsym=pthread_create=__interceptor_pthread_create");
    }
}
