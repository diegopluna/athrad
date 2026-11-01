//! `athrad FILE.rs [-o OUTPUT]`: build a one-file binary crate, as
//! `rustc --edition 2024 FILE.rs` would.

#![feature(rustc_private)]

extern crate rustc_driver;

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let mut source = None;
    let mut output = None;
    while let Some(arg) = args.next() {
        if arg == "-o" {
            output = args.next().map(PathBuf::from);
        } else {
            source = Some(PathBuf::from(arg));
        }
    }
    let Some(source) = source else {
        eprintln!("usage: athrad FILE.rs [-o OUTPUT]");
        return ExitCode::from(2);
    };
    let stem = source.file_stem().and_then(|s| s.to_str()).unwrap_or("main").to_owned();
    let output = output.unwrap_or_else(|| PathBuf::from(link::executable_name(&stem)));
    let work_dir = std::env::temp_dir().join(format!("athrad-{stem}-{}", std::process::id()));
    let result = link::HostCc::new().and_then(|linker| athrad::build(&source, &linker, &work_dir, &output));
    let _ = std::fs::remove_dir_all(&work_dir);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
