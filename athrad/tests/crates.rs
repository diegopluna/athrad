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
        let may_reject = cfg!(windows)
            && std::fs::read_to_string(krate.dir.join("Cargo.toml"))
                .is_ok_and(|s| s.contains("athrad: may-reject-on-windows"));
        match oracle::check_crate(&krate, athrad, work) {
            Ok(agreed) => eprintln!("{}: {agreed} builds agree", krate.name),
            Err(e) if may_reject && e.is::<oracle::Rejected>() => eprintln!("{}: rejected here", krate.name),
            Err(e) => panic!("{e:#}"),
        }
    }
}
