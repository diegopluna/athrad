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

#[test]
fn print_edges() {
    let agreed = oracle::check(&programs::PRINT_EDGES, work_dir()).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(agreed.rustc.stdout, b"0\n7\n-42\n1000000\n9223372036854775807\n-9223372036854775808\n");
}

/// The oracle must notice when Athrad is wrong: same source, IR that adds 2 and 4.
#[test]
fn catches_a_disagreement() {
    fn wrong() -> ir::Module {
        use ir::{FunctionBuilder, Linkage, Signature, Type};
        let mut m = ir::Module::new();
        let mut b = FunctionBuilder::new(athrad::rt::USER_MAIN, Linkage::Local, Signature::default());
        let six = b.iconst(Type::I64, 6);
        b.call(athrad::rt::PRINTLN_I64, &[six], None);
        b.ret(None);
        m.push(b.finish());
        m
    }
    let program = oracle::Program { name: "add_wrong", ir: wrong, ..programs::ADD };
    let err = oracle::check(&program, work_dir()).expect_err("the oracle accepted 6 for 2 + 3");
    assert!(format!("{err:#}").contains("disagrees"), "{err:#}");
}
