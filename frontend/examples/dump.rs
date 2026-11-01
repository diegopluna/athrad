#![feature(rustc_private)]
extern crate rustc_driver;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    frontend::dump_mir(&args);
}
