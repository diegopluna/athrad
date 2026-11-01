//! Wall time of Athrad's back half (IR -> Cranelift -> object -> host link)
//! on the add program. Not comparable to `cargo build` until milestone 1
//! puts rustc's frontend in front of it; it is the floor that frontend adds to.
//!
//! cargo run --release -p oracle --example time_athrad

use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let program = &oracle::programs::ADD;
    let dir = std::env::temp_dir().join("athrad-time");
    let exe = dir.join(link::executable_name(program.name));
    let module = (program.ir)();
    let runs = 20;
    let mut total = Vec::new();
    let mut codegen = Vec::new();
    for _ in 0..runs {
        let t0 = Instant::now();
        let mut m = module.clone();
        athrad::rt::add_runtime(&mut m, link::ObjectFormat::host());
        ir::verify(&m)?;
        let obj = codegen::compile(&m, program.name)?;
        let t1 = Instant::now();
        athrad::build(&module, &dir, &exe)?;
        let t2 = Instant::now();
        codegen.push((t1 - t0).as_secs_f64() * 1e3);
        total.push((t2 - t1).as_secs_f64() * 1e3);
        drop(obj);
    }
    let median = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    println!("verify + codegen: {:.2} ms (median of {runs})", median(&mut codegen));
    println!("athrad::build (verify, codegen, write, link): {:.1} ms (median of {runs})", median(&mut total));
    Ok(())
}
