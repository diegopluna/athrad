fn main() {
    let mut rng = rng::Rng::new(4);
    let mut a: Vec<u64> = (0..3_000_000).map(|_| rng.next()).collect();
    a.sort_unstable();
    let mut b: Vec<(u32, u32)> = (0..2_000_000).map(|_| (rng.below(1000) as u32, rng.next() as u32)).collect();
    b.sort();
    let mut s: Vec<String> = (0..400_000).map(|_| rng.word()).collect();
    s.sort();
    s.dedup();
    let mut c: Vec<i64> = (0..2_000_000).map(|_| rng.next() as i64).collect();
    c.sort_by_key(|x| x.wrapping_mul(31) % 1000);
    println!("{} {} {} {} {}", a[1234], b[5678].1, s.len(), s[s.len() / 2], c[999]);
}
