//! Every program in `oracle/programs` must behave the same built by `rustc`
//! and by Athrad, on this host.

// Athrad runs rustc in-process, so everything that links it links rustc_driver.
#![feature(rustc_private)]

extern crate rustc_driver;

use std::path::Path;

fn work_dir() -> &'static Path {
    Path::new(env!("CARGO_TARGET_TMPDIR"))
}

fn check(name: &str) -> oracle::Agreement {
    let program = oracle::program(name).unwrap();
    oracle::check(&program, work_dir()).unwrap_or_else(|e| panic!("{e:#}"))
}

#[test]
fn add() {
    assert_eq!(check("add").rustc.stdout, b"5\n");
}

#[test]
fn overflow_panics() {
    let agreed = check("overflow");
    assert_eq!(agreed.rustc.code, Some(101));
    assert!(String::from_utf8_lossy(&agreed.rustc.stderr).contains("attempt to add with overflow"));
}

#[test]
fn every_program() {
    let mut failures = Vec::new();
    for program in oracle::programs().unwrap() {
        if let Err(e) = oracle::check(&program, &work_dir().join("all")) {
            failures.push(format!("{e:#}"));
        }
    }
    assert!(failures.is_empty(), "{} failed:\n\n{}", failures.len(), failures.join("\n\n"));
}

/// Programs Athrad does not build yet. Rejecting one is fine; building one
/// that disagrees with `rustc` is not. Each one Athrad now agrees on is
/// listed, to be moved up into `oracle/programs`.
#[test]
fn later_programs_never_disagree() {
    let mut disagreements = Vec::new();
    let mut ready = Vec::new();
    for program in oracle::later_programs().unwrap() {
        match oracle::check(&program, &work_dir().join("later")) {
            Ok(_) => ready.push(program.name),
            Err(e) if e.is::<oracle::Rejected>() => {}
            Err(e) => disagreements.push(format!("{e:#}")),
        }
    }
    if !ready.is_empty() {
        eprintln!("now agree with rustc, move to oracle/programs: {}", ready.join(", "));
    }
    assert!(disagreements.is_empty(), "{}", disagreements.join("\n\n"));
}

/// The oracle must notice when Athrad is wrong: rustc builds `add` (prints
/// 5), Athrad builds a program that prints 6.
#[test]
fn catches_a_disagreement() {
    let dir = work_dir().join("wrong");
    std::fs::create_dir_all(&dir).unwrap();
    let wrong = dir.join("six.rs");
    std::fs::write(&wrong, "fn main() {\n    println!(\"{}\", 6);\n}\n").unwrap();
    let add = oracle::program("add").unwrap();
    let err = oracle::check_sources("add_wrong", &add.path, &wrong, &dir).expect_err("the oracle accepted 6 for 2 + 3");
    assert!(format!("{err:#}").contains("disagrees"), "{err:#}");
}
