// `athrad` links `rustc_driver`, a dylib in the toolchain's sysroot. Record
// where it is, so the binary runs outside `cargo run` too. Windows finds the
// DLL through PATH (the toolchain's `bin`), which rustup's proxies set.
//
// This script links `frontend`, which is a `rustc_private` crate, so it is
// one too. Without `rustc_driver` here, `std` shows up twice.
#![feature(rustc_private)]

extern crate rustc_driver;

fn main() {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let out = std::process::Command::new(rustc).args(["--print", "sysroot"]).output().expect("running rustc");
    let sysroot = String::from_utf8(out.stdout).expect("utf-8 sysroot");
    let target = std::env::var("TARGET").unwrap_or_default();
    // Where `athrad` finds the toolchain's `llvm-profdata`, with no `rustc`
    // to ask at run time.
    println!("cargo:rustc-env=ATHRAD_SYSROOT={}", sysroot.trim());
    println!("cargo:rustc-env=ATHRAD_HOST={target}");
    if !target.contains("windows") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{}/lib", sysroot.trim());
    }
    println!("cargo:rerun-if-changed=build.rs");
    embed_runtime();
}

/// Compile the subset's runtime and leave its objects, its link inputs, and
/// what it learned about `std` where the crate can include them.
fn embed_runtime() {
    println!("cargo:rerun-if-changed=rt/athrad_runtime.rs");
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let src = out_dir.join("athrad_runtime.rs");
    std::fs::write(&src, std::fs::read_to_string("rt/athrad_runtime.rs").expect("reading the runtime source")).unwrap();
    let work = out_dir.join("work");
    let _ = std::fs::remove_dir_all(&work);
    let codegen: frontend::Codegen = std::sync::Arc::new({
        let work = work.clone();
        move |module: &ir::Module, unit: &str| {
            let format = link::ObjectFormat::host();
            let obj = codegen::compile(module, unit)?;
            let path = work.join(format!("{unit}.{}", format.extension()));
            link::write_object(&obj, format, &path)?;
            Ok(path)
        }
    });
    let args = vec![
        "athrad".into(),
        "--edition".into(),
        "2024".into(),
        "--crate-type".into(),
        "bin".into(),
        "--crate-name".into(),
        "athrad_runtime".into(),
        src.display().to_string(),
    ];
    let out = frontend::compile_with(&args, &work, codegen, true).expect("building the subset runtime");
    let mut link_inputs = out.link.expect("the runtime is an executable");
    link_inputs.objects.clear();
    let mut paths: Vec<std::path::PathBuf> = out.objects.iter().map(|(_, path)| path.clone()).collect();
    if let Some(shim) = out.allocator {
        paths.push(shim);
    }
    let mut files = Vec::new();
    for path in &paths {
        let name = format!("rt{}.o", files.len());
        std::fs::copy(path, out_dir.join(&name)).unwrap();
        files.push(name);
    }
    let abi = out.std_abi.expect("the runtime reads std's ABI");
    std::fs::write(out_dir.join("abi.bin"), postcard::to_allocvec(&abi).unwrap()).unwrap();
    std::fs::write(out_dir.join("link.bin"), postcard::to_allocvec(&link_inputs).unwrap()).unwrap();
    let mut generated = String::from("pub static OBJECTS: &[&[u8]] = &[\n");
    for name in &files {
        generated.push_str(&format!("    include_bytes!({name:?}),\n"));
    }
    generated.push_str("];\n");
    generated.push_str("pub static ABI: &[u8] = include_bytes!(\"abi.bin\");\n");
    generated.push_str("pub static LINK: &[u8] = include_bytes!(\"link.bin\");\n");
    std::fs::write(out_dir.join("runtime_gen.rs"), generated).unwrap();
}
