//! Athrad's own frontend, for a subset of Rust: milestone 4.
//!
//! The subset is what a one-file program needs to compute with integers and
//! print the results: `fn` items with integer and `bool` parameters, `let`
//! and `let mut`, arithmetic, comparisons, `if`, `while`, `loop`, `for` over
//! integer ranges, `break`, `continue`, `return`, calls between the
//! program's functions, `i64::MAX` and friends, `as` between integers,
//! `std::hint::black_box`, and `print!`, `println!`, `eprint!`,
//! `eprintln!` with `{}`, `{:?}`, `{0}`, and `{name}`.
//!
//! The rule is the handoff's: a program Athrad accepts must be valid Rust and
//! behave as `rustc` would make it behave. So anything this frontend is not
//! sure of, from syntax it does not know to a type error to an overflow
//! `rustc` would catch at compile time, is a [`Fallback`]: the program goes
//! to `rustc`, as every program did before, and `rustc` compiles it or
//! reports the error. This frontend never writes a diagnostic.
//!
//! The programs it compiles link with a runtime Athrad builds once with
//! `rustc` (see `athrad::runtime`): `std`, `lang_start`, the C `main`, and
//! the `print!` helpers this frontend calls. What it needs to know about the
//! pinned `std`, which panic functions to call and how a `Location` is laid
//! out, comes from there too, as an [`Abi`].

mod check;
mod lint;
mod lower;
mod syntax;

use std::fmt;

pub use lower::USER_MAIN;

/// Why a program goes to `rustc` instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fallback(pub String);

impl fmt::Display for Fallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the subset does not cover {}", self.0)
    }
}

impl std::error::Error for Fallback {}

pub(crate) fn fallback(what: impl Into<String>) -> Fallback {
    Fallback(what.into())
}

/// The pinned `std`, as this frontend needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Abi {
    /// Symbols of `core`'s panic functions, by lang item name
    /// (`panic_const_add_overflow`, ...). Each takes a `&Location`.
    pub panics: Vec<(String, String)>,
    pub location: Location,
}

impl Abi {
    fn panic(&self, item: &str) -> Option<&str> {
        self.panics.iter().find(|(k, _)| k == item).map(|(_, s)| s.as_str())
    }
}

/// The layout of `core::panic::Location`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub size: u64,
    pub align: u64,
    pub file_ptr: u64,
    pub file_len: u64,
    pub file_nul: bool,
    pub line: u64,
    pub col: u64,
}

/// Compile `source`, the text of the file `file` names as `rustc` would be
/// given it, to IR whose `athrad_user_main` is the program's `main`.
pub fn compile(source: &str, file: &str, abi: &Abi) -> Result<ir::Module, Fallback> {
    // Columns in panic locations are what rustc displays; tabs and wide
    // characters make those differ from counting characters.
    if source.contains('\t') || !source.is_ascii() {
        return Err(fallback("tabs and characters outside ASCII"));
    }
    let funcs = syntax::parse(source)?;
    let typed = check::check(&funcs)?;
    lint::lint(&funcs, &typed)?;
    lower::lower(&funcs, &typed, abi, file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abi() -> Abi {
        let items = ["add", "sub", "mul", "div", "rem", "neg", "shl", "shr"];
        let mut panics: Vec<(String, String)> =
            items.iter().map(|i| (format!("panic_const_{i}_overflow"), format!("p_{i}"))).collect();
        panics.push(("panic_const_div_by_zero".into(), "p_div0".into()));
        panics.push(("panic_const_rem_by_zero".into(), "p_rem0".into()));
        Abi {
            panics,
            location: Location { size: 24, align: 8, file_ptr: 0, file_len: 8, file_nul: false, line: 16, col: 20 },
        }
    }

    fn accepts(src: &str) {
        let m = compile(src, "t.rs", &abi()).unwrap_or_else(|e| panic!("{e}: {src}"));
        ir::verify(&m).unwrap();
    }

    fn falls_back(src: &str) {
        assert!(compile(src, "t.rs", &abi()).is_err(), "accepted: {src}");
    }

    #[test]
    fn accepts_the_subset() {
        accepts("fn main() { println!(\"{}\", 1 + 2); }");
        accepts(
            "fn f(x: u64) -> u64 { x * 2 } fn main() { let mut i = 0; while i < 3 { print!(\"{} \", f(i)); i += 1; } println!(); }",
        );
        accepts("fn main() { for i in 0..=3u8 { if i == 2 { continue; } println!(\"{i}\"); } }");
        accepts("fn main() { let x = -128i8; let y = i64::MIN; println!(\"{x} {y:?}\"); }");
        accepts("fn f() -> u32 { loop { return 3; } } fn main() { println!(\"{}\", f()); }");
        accepts("fn main() { let x = std::hint::black_box(5u8); println!(\"{}\", x << 3u32); }");
    }

    /// Programs `rustc` rejects, and ones Athrad cannot be sure of: each
    /// must go to `rustc`.
    #[test]
    fn falls_back_on_what_rustc_rejects() {
        // Types, names, mutability.
        falls_back("fn main() { let x: u32 = true; }");
        falls_back("fn main() { let x = 1; x = 2; }");
        falls_back("fn main() { println!(\"{}\", y); }");
        falls_back("fn f(x: u8) {} fn main() { f(1u16); }");
        falls_back("fn f() -> u32 { if true { return 1; } } fn main() {}");
        falls_back("fn main() { let x = -1u32; }");
        falls_back("fn main() { let x: u32 = 5; let y = -x; }");
        falls_back("fn main() { break; }");
        falls_back("fn main() { println!(\"{} {}\", 1); }");
        falls_back("fn main() { println!(\"{}\", 1, 2); }");
        // overflowing_literals.
        falls_back("fn main() { let x: u8 = 256; }");
        falls_back("fn main() { let x = 2147483648; }");
        falls_back("fn main() { let x = -129i8; }");
        falls_back("fn main() { let x = 300 as u8; }");
        // arithmetic_overflow and unconditional_panic, which rustc finds by
        // propagating constants.
        falls_back("fn main() { let x = i32::MAX + 1; }");
        falls_back("fn main() { let x = 255u8; let y = x + 1; }");
        falls_back("fn main() { let mut x = 255u8; x += 1; }");
        falls_back("fn main() { let x = 1i32 << 40; }");
        falls_back("fn main() { let x = std::hint::black_box(1u8); let y = x >> 8; }");
        falls_back("fn main() { let x = std::hint::black_box(7); let y = x / 0; }");
        falls_back("fn main() { let x = i64::MIN / -1; }");
        falls_back("fn main() { let x = -i8::MIN; }");
        falls_back("fn main() { let a = 200u8; let b = if a > 100 { a * 2 } else { 0 }; }");
        // Outside the subset.
        falls_back("#![deny(warnings)] fn main() {}");
        falls_back("fn main() { let v = vec![1]; }");
        falls_back("fn main() { let x = 1.5; }");
        falls_back("fn main() { let s = \"x\"; }");
        falls_back("fn main() -> u8 { 0 }");
        falls_back("use std::fmt; fn main() {}");
        falls_back("fn main() {\tprintln!(); }");
    }
}
