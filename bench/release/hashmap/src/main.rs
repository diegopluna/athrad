use std::collections::{HashMap, HashSet};

fn main() {
    let mut rng = rng::Rng::new(3);
    let mut counts: HashMap<u64, u64> = HashMap::new();
    for _ in 0..4_000_000 {
        *counts.entry(rng.below(200_000)).or_insert(0) += 1;
    }
    let mut words: HashMap<String, usize> = HashMap::new();
    for _ in 0..600_000 {
        *words.entry(rng.word()).or_default() += 1;
    }
    let mut seen = HashSet::new();
    let mut dup = 0u64;
    for _ in 0..2_000_000 {
        if !seen.insert(rng.below(1_500_000)) {
            dup += 1;
        }
    }
    let hits: u64 = (0..2_000_000u64).filter_map(|k| counts.get(&k)).sum();
    println!("{} {} {} {}", counts.len(), words.len(), dup, hits);
}
