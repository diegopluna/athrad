#[derive(Debug, Clone, Copy)]
struct V2 {
    x: f32,
    y: f32,
}

#[derive(Debug, Clone, Copy)]
struct V3 {
    x: f64,
    y: f64,
    z: f64,
}

fn len2(v: V2) -> f32 {
    (v.x * v.x + v.y * v.y).sqrt()
}

fn scale(v: V3, k: f64) -> V3 {
    V3 { x: v.x * k, y: v.y * k, z: v.z * k }
}

fn split(x: f64) -> (f64, f64) {
    (x.trunc(), x.fract())
}

fn mixed(a: i32, b: f32, c: u8, d: f64, e: i64) -> f64 {
    a as f64 + b as f64 + c as f64 + d + e as f64
}

fn main() {
    let v = V2 { x: 3.0, y: 4.0 };
    println!("{} {:?}", len2(v), v);
    println!("{:?}", scale(V3 { x: 1.0, y: -2.5, z: 1e300 }, 10.0));
    println!("{:?} {:?}", split(7.75), split(-2.5));
    println!("{}", mixed(-1, 0.5, 200, 1e-3, 1 << 40));
    let nan = f64::NAN;
    let inf = f64::INFINITY;
    println!("{} {} {} {}", nan, inf, -inf, -0.0f64);
    println!("{} {} {}", nan == nan, nan != nan, nan < 1.0);
    println!("{} {} {}", 1.0f64.min(nan), nan.max(2.0), 3.5f32.min(-1.0));
    println!("{} {} {} {}", (300.7f64) as u8, (-5.9f64) as u8, nan as i32, inf as i64);
    println!("{} {} {}", (-1e20f32) as i16, u64::MAX as f32, i64::MIN as f64);
    println!("{} {} {}", 2.0f64.powi(10), 2.0f64.powf(0.5), 10f64.ln());
    println!("{} {} {}", 1.5f64.round(), 2.5f64.round(), (-2.5f64).round_ties_even());
    println!("{} {} {}", 7.5f64 % 2.0, -7.5f32 % 2.0, 1.0f64.copysign(-0.0));
    println!("{} {} {}", 0.1f64 + 0.2, 1.0f32 / 3.0, (2.0f64).sqrt());
    println!("{:.3} {:e} {:10.2}|", std::f64::consts::PI, 1234.5f64, -0.125);
    println!("{} {}", f32::MAX, f64::MIN_POSITIVE);
    let xs = [3.5f64, -1.25, 9.0, 0.0];
    let sum: f64 = xs.iter().sum();
    let max = xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    println!("{sum} {max} {:?}", xs.iter().map(|x| x.abs().floor()).collect::<Vec<_>>());
    println!("{}", (1.0f64).mul_add(2.0, 3.0));
    let bits = 1.5f64.to_bits();
    println!("{bits:x} {}", f64::from_bits(bits + 1));
}
