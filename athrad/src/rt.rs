//! The runtime, written in Athrad IR: what `std` does for the programs the
//! pipeline can run so far.
//!
//! It calls only C library functions that exist on all three hosts
//! (`putchar`, and `_setmode` on Windows), so no part of it is a C backend.

use ir::{BinOp, Cmp, FunctionBuilder, Linkage, Module, Signature, Type};
use link::ObjectFormat;

/// The body of the Rust program's `fn main`. The program defines it.
pub const USER_MAIN: &str = "athrad_user_main";
/// `println!("{}", n)` for an `i64` `n`.
pub const PRINTLN_I64: &str = "athrad_rt_println_i64";

const PRINT_U64: &str = "athrad_rt_print_u64";
const PUTCHAR: &str = "putchar";
const SETMODE: &str = "_setmode";

/// Add the runtime functions and the C `main` to `module`.
pub fn add_runtime(module: &mut Module, format: ObjectFormat) {
    module.declare_extern(PUTCHAR, Signature::new([Type::I32], Some(Type::I32)));
    if format == ObjectFormat::Coff {
        module.declare_extern(SETMODE, Signature::new([Type::I32, Type::I32], Some(Type::I32)));
    }
    module.push(c_main(format));
    module.push(print_u64());
    module.push(println_i64());
}

/// `int main(void)`: set stdout up, run the program, exit 0.
fn c_main(format: ObjectFormat) -> ir::Function {
    let mut b = FunctionBuilder::new("main", Linkage::Export, Signature::new([], Some(Type::I32)));
    if format == ObjectFormat::Coff {
        // Rust writes `\n`, not `\r\n`. The C runtime's stdout starts in text
        // mode on Windows; put it in binary mode (`_O_BINARY`) to match.
        let fd = b.iconst(Type::I32, 1);
        let mode = b.iconst(Type::I32, 0x8000);
        b.call(SETMODE, &[fd, mode], Some(Type::I32));
    }
    b.call(USER_MAIN, &[], None);
    let zero = b.iconst(Type::I32, 0);
    b.ret(Some(zero));
    b.finish()
}

/// Print an unsigned 64-bit integer in decimal, most significant digit first.
fn print_u64() -> ir::Function {
    let mut b = FunctionBuilder::new(PRINT_U64, Linkage::Local, Signature::new([Type::I64], None));
    let n = b.params()[0];
    let ten = b.iconst(Type::I64, 10);
    let rest = b.binary(BinOp::UDiv, n, ten);
    let digit = b.binary(BinOp::URem, n, ten);
    let zero = b.iconst(Type::I64, 0);
    let more = b.cmp(Cmp::Ne, rest, zero);
    let (recurse, _) = b.block(&[]);
    let (emit, _) = b.block(&[]);
    b.branch(more, (recurse, &[]), (emit, &[]));

    b.switch_to(recurse);
    b.call(PRINT_U64, &[rest], None);
    b.jump(emit, &[]);

    b.switch_to(emit);
    let ascii_zero = b.iconst(Type::I64, i64::from(b'0'));
    let ch = b.add(digit, ascii_zero);
    let ch = b.trunc(Type::I32, ch);
    b.call(PUTCHAR, &[ch], Some(Type::I32));
    b.ret(None);
    b.finish()
}

/// Print a signed 64-bit integer and a newline. `i64::MIN` works: its
/// wrapping negation, read as unsigned, is its magnitude.
fn println_i64() -> ir::Function {
    let mut b = FunctionBuilder::new(PRINTLN_I64, Linkage::Local, Signature::new([Type::I64], None));
    let n = b.params()[0];
    let zero = b.iconst(Type::I64, 0);
    let negative = b.cmp(Cmp::SLt, n, zero);
    let (minus, _) = b.block(&[]);
    let (digits, magnitude) = b.block(&[Type::I64]);
    b.branch(negative, (minus, &[]), (digits, &[n]));

    b.switch_to(minus);
    let dash = b.iconst(Type::I32, i64::from(b'-'));
    b.call(PUTCHAR, &[dash], Some(Type::I32));
    let neg = b.sub(zero, n);
    b.jump(digits, &[neg]);

    b.switch_to(digits);
    b.call(PRINT_U64, &[magnitude[0]], None);
    let newline = b.iconst(Type::I32, i64::from(b'\n'));
    b.call(PUTCHAR, &[newline], Some(Type::I32));
    b.ret(None);
    b.finish()
}
