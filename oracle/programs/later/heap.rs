use std::collections::HashMap;

fn main() {
    let mut v: Vec<u64> = Vec::new();
    for i in 0..10 {
        v.push(i * i);
    }
    println!("{v:?} len={} sum={}", v.len(), v.iter().sum::<u64>());
    let evens: Vec<u64> = v.iter().copied().filter(|x| x % 2 == 0).collect();
    println!("{evens:?}");

    let mut s = String::from("hello");
    s.push_str(", world");
    s += "!";
    println!("{s} {} {}", s.len(), s.to_uppercase());
    let words: Vec<&str> = s.split(' ').collect();
    println!("{words:?}");

    let b = Box::new([1u8, 2, 3]);
    println!("{:?} {}", b, b.len());

    let mut m = HashMap::new();
    for w in "the cat and the hat and the bat".split_whitespace() {
        *m.entry(w).or_insert(0) += 1;
    }
    let mut counts: Vec<_> = m.into_iter().collect();
    counts.sort();
    println!("{counts:?}");
}
