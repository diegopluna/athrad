use std::collections::{HashMap, HashSet};

fn main() {
    let mut m = HashMap::new();
    for w in "the cat and the hat and the bat".split_whitespace() {
        *m.entry(w).or_insert(0) += 1;
    }
    let mut counts: Vec<_> = m.into_iter().collect();
    counts.sort();
    println!("{counts:?}");
    let s: HashSet<u32> = (0..100).map(|x| x % 7).collect();
    let mut v: Vec<_> = s.into_iter().collect();
    v.sort_unstable();
    println!("{v:?}");
}
