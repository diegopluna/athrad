use std::process::ExitCode;

fn main() -> ExitCode {
    eprintln!("to stderr");
    println!("to stdout");
    if std::hint::black_box(true) { ExitCode::from(42) } else { ExitCode::SUCCESS }
}
