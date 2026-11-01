// A build id for the per-function code cache: a hash of the sources that
// decide what machine code a function gets (`ir`, `codegen`) and of
// Cargo.lock, which pins Cranelift. A change to any of them misses every
// entry an older Athrad wrote.
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut h = DefaultHasher::new();
    let mut files = Vec::new();
    for dir in [root.join("src"), root.join("../ir/src")] {
        println!("cargo:rerun-if-changed={}", dir.display());
        for entry in std::fs::read_dir(&dir).expect("source dir") {
            files.push(entry.expect("dir entry").path());
        }
    }
    let lock = root.join("../Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock.display());
    files.push(lock);
    files.sort();
    for f in files {
        f.file_name().hash(&mut h);
        std::fs::read(&f).expect("readable source").hash(&mut h);
    }
    println!("cargo:rustc-env=ATHRAD_CODEGEN_BUILD_ID={:016x}", h.finish());
}
