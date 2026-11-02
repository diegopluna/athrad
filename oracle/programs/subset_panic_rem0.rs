fn compute() -> i128 {
    let before = std::hint::black_box(3);
    println!("before {before}");
    let v = 10u64 % std::hint::black_box(0u64);
    v as i128
}

fn main() {
    println!("{}", compute());
}
