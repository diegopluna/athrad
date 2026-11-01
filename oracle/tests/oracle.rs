//! Every program in `oracle::programs::ALL` must behave the same built by
//! `rustc` and by Athrad, on this host.

use std::path::Path;

use oracle::programs;

fn work_dir() -> &'static Path {
    Path::new(env!("CARGO_TARGET_TMPDIR"))
}

#[test]
fn add() {
    let agreed = oracle::check(&programs::ADD, work_dir()).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(agreed.rustc.stdout, b"5\n");
}

#[test]
fn every_program() {
    let mut failures = Vec::new();
    for program in programs::ALL {
        if let Err(e) = oracle::check(program, &work_dir().join("all")) {
            failures.push(format!("{e:#}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
