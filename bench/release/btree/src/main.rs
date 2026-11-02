use std::collections::{BTreeMap, BTreeSet, VecDeque};

fn main() {
    let mut rng = rng::Rng::new(8);
    let mut m = BTreeMap::new();
    for i in 0..1_500_000u64 {
        m.insert(rng.below(3_000_000), i);
    }
    let range: u64 = m.range(1_000_000..1_200_000).map(|(_, v)| v % 7).sum();
    let mut set = BTreeSet::new();
    for _ in 0..500_000 {
        set.insert(rng.word());
    }
    let mut q = VecDeque::new();
    let mut acc = 0u64;
    for i in 0..3_000_000u64 {
        q.push_back(i ^ rng.below(1 << 20));
        if q.len() > 1000 {
            acc = acc.wrapping_add(q.pop_front().unwrap());
        }
    }
    for k in 0..200_000u64 {
        m.remove(&(k * 13));
    }
    println!("{} {} {} {} {}", m.len(), range, set.len(), set.iter().nth(1000).unwrap(), acc);
}
