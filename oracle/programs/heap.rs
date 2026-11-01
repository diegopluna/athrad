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

    let mut pairs = vec![("b", 2), ("a", 9), ("c", 1), ("a", 3)];
    pairs.sort();
    println!("{pairs:?}");
    pairs.sort_by_key(|p| p.1);
    println!("{pairs:?} {:?}", pairs.iter().max_by_key(|p| p.1));
    let nested: Vec<Vec<String>> = (1..4).map(|n| (0..n).map(|i| i.to_string()).collect()).collect();
    println!("{nested:?}");
}
