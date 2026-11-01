//! Wall time of Athrad's back half (IR -> Cranelift -> object -> host link)
//! on the add program, phase by phase. Not comparable to `cargo build` until
//! milestone 1 puts rustc's frontend in front of it; it is the floor that
//! frontend adds to.
//!
//! cargo run --release -p oracle --example time_athrad

use std::time::Instant;

use link::Linker;

fn main() -> anyhow::Result<()> {
    let program = &oracle::programs::ADD;
    let dir = std::env::temp_dir().join("athrad-time");
    std::fs::create_dir_all(&dir)?;
    let format = link::ObjectFormat::host();
    let exe = dir.join(link::executable_name(program.name));
    let obj_path = dir.join(format!("{}.{}", program.name, format.extension()));
    let module = (program.ir)();
    let runs = 20;

    let phases = ["verify + codegen", "write object", "find linker", "link", "athrad::build (linker found)"];
    let mut times: Vec<Vec<f64>> = vec![Vec::new(); phases.len()];
    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1e3;
    for _ in 0..runs {
        let t = Instant::now();
        let mut m = module.clone();
        athrad::rt::add_runtime(&mut m, format);
        ir::verify(&m)?;
        let obj = codegen::compile(&m, program.name)?;
        times[0].push(ms(t));

        let t = Instant::now();
        link::write_object(&obj, format, &obj_path)?;
        times[1].push(ms(t));

        let t = Instant::now();
        let linker = link::HostCc::new()?;
        times[2].push(ms(t));

        let t = Instant::now();
        linker.link(std::slice::from_ref(&obj_path), &exe)?;
        times[3].push(ms(t));

        let t = Instant::now();
        athrad::build(&module, &linker, &dir, &exe)?;
        times[4].push(ms(t));
    }
    for (name, v) in phases.iter().zip(&mut times) {
        v.sort_by(f64::total_cmp);
        println!("{name:28} {:7.2} ms  (median of {runs})", v[v.len() / 2]);
    }
    Ok(())
}
