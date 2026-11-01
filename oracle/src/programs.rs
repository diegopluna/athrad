//! The programs Athrad must agree with `rustc` on, each as Rust source and
//! as the IR Athrad builds from it.

use athrad::rt;
use ir::{FunctionBuilder, Linkage, Module, Signature, Type};

use crate::Program;

pub const ALL: &[Program] = &[ADD];

/// Milestone 0: a function that adds two numbers, and a print of the result.
pub const ADD: Program = Program { name: "add", source: include_str!("../programs/add.rs"), ir: add };

fn add() -> Module {
    let mut m = Module::new();

    // fn add(a: i64, b: i64) -> i64 { a + b }
    //
    // rustc checks this add for overflow in a debug build. With the constant
    // arguments below it cannot overflow; once IR comes from MIR, the check
    // comes with it.
    let mut b = FunctionBuilder::new("add", Linkage::Local, Signature::new([Type::I64, Type::I64], Some(Type::I64)));
    let p = b.params();
    let sum = b.add(p[0], p[1]);
    b.ret(Some(sum));
    m.push(b.finish());

    // fn main() { println!("{}", add(2, 3)); }
    let mut b = FunctionBuilder::new(rt::USER_MAIN, Linkage::Local, Signature::default());
    let two = b.iconst(Type::I64, 2);
    let three = b.iconst(Type::I64, 3);
    let sum = b.call("add", &[two, three], Some(Type::I64)).expect("add returns i64");
    b.call(rt::PRINTLN_I64, &[sum], None);
    b.ret(None);
    m.push(b.finish());

    m
}
