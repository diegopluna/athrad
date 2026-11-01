fn add(a: i64, b: i64) -> i64 {
    a + b
}

fn main() {
    let big = std::hint::black_box(i64::MAX);
    println!("before");
    println!("{}", add(big, 1));
}
