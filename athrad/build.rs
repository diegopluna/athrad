// `athrad` links `rustc_driver`, a dylib in the toolchain's sysroot. Record
// where it is, so the binary runs outside `cargo run` too. Windows finds the
// DLL through PATH (the toolchain's `bin`), which rustup's proxies set.
fn main() {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let out = std::process::Command::new(rustc).args(["--print", "sysroot"]).output().expect("running rustc");
    let sysroot = String::from_utf8(out.stdout).expect("utf-8 sysroot");
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("windows") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{}/lib", sysroot.trim());
    }
    println!("cargo:rerun-if-changed=build.rs");
}
