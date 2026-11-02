fn main() {
    let mut buf = itoa::Buffer::new();
    for n in [0u64, 7, 42, 1_000_000, u64::MAX] {
        println!("{}", buf.format(n));
    }
    for n in [i128::MIN, -1, i128::MAX] {
        println!("{}", buf.format(n));
    }
    let v: Vec<String> = (0u16..5).map(|i| itoa::Buffer::new().format(i * 1000).to_owned()).collect();
    println!("{}", v.join(","));
    let at = std::hint::black_box(9);
    println!("{}", v[at]);
}
