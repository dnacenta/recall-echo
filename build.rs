// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `cfg(embed_tests)` when `RECALL_ECHO_MODELS_DIR` is set and non-empty.
//!
//! Tests that embed are ignored without it, so a plain `cargo test` never
//! downloads the embedding model.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(embed_tests)");
    println!("cargo:rerun-if-env-changed=RECALL_ECHO_MODELS_DIR");
    if std::env::var_os("RECALL_ECHO_MODELS_DIR").is_some_and(|dir| !dir.is_empty()) {
        println!("cargo:rustc-cfg=embed_tests");
    }
}
