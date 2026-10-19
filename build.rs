// SPDX-License-Identifier: GPL-3.0-or-later

//! Sets `cfg(neon)` for 32-bit ARM builds with NEON, which stable Rust
//! doesn't expose as `cfg(target_feature = "neon")`.

use std::env;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(neon)");
    println!("cargo::rerun-if-env-changed=CARGO_ENCODED_RUSTFLAGS");
    let arm = env::var("CARGO_CFG_TARGET_ARCH").is_ok_and(|a| a == "arm");
    let target = env::var("TARGET").unwrap_or_default();
    let flags = env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default();
    // `-C target-feature=+neon`, as the ARMv7 release is built.
    let enabled = flags
        .split('\x1f')
        .any(|f| f.contains("target-feature=") && f.split(['=', ',']).any(|x| x == "+neon"));
    if arm && (target.contains("neon") || enabled) {
        println!("cargo::rustc-cfg=neon");
    }
}
