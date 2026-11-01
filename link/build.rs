// Record the target triple Athrad itself was built for. Athrad compiles for
// the host it runs on, and the `cc` crate needs the triple to find the right
// toolchain (on Windows, the MSVC linker and its library paths).
fn main() {
    let target = std::env::var("TARGET").expect("cargo sets TARGET");
    println!("cargo:rustc-env=ATHRAD_HOST_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
