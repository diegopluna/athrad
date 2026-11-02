use std::fmt::Write;

fn main() {
    let mut rng = rng::Rng::new(9);
    let mut out = String::new();
    for i in 0..600_000u64 {
        write!(out, "{}:{}:{:x};", i, rng.word(), rng.next()).unwrap();
    }
    let upper = out.to_uppercase();
    let parts: Vec<&str> = upper.split(';').collect();
    let numbers: u64 = parts.iter().filter_map(|p| p.split(':').next()?.parse::<u64>().ok()).sum();
    let replaced = out.replace("ab", "XYZ");
    let words = out.split(|c: char| !c.is_ascii_alphabetic()).filter(|w| w.len() > 5).count();
    let mut floats = String::new();
    for i in 0..300_000 {
        write!(floats, "{:.3} ", (i as f64).sqrt() * 1.618).unwrap();
    }
    let fsum: f64 = floats.split_whitespace().map(|s| s.parse::<f64>().unwrap()).sum();
    println!("{} {} {} {} {:.3}", parts.len(), numbers, replaced.len(), words, fsum);
}
