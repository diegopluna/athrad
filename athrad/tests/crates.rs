//! Milestone 3 under the oracle: Cargo projects in `oracle/crates`, with
//! modules and dependencies, built by `cargo build` and by `athrad build`,
//! and rebuilt by both after each of their edits.

#![feature(rustc_private)]

extern crate rustc_driver;

use std::path::Path;

#[test]
fn cargo_projects() {
    let athrad = Path::new(env!("CARGO_BIN_EXE_athrad"));
    let work = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let crates = oracle::crates().unwrap();
    assert!(!crates.is_empty());
    for krate in crates {
        let agreed = oracle::check_crate(&krate, athrad, work).unwrap_or_else(|e| panic!("{e:#}"));
        eprintln!("{}: {agreed} builds agree", krate.name);
    }
}
