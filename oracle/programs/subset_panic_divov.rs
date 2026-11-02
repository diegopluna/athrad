fn compute() -> i128 {
    let before = std::hint::black_box(3);
    println!("before {before}");
    let v = std::hint::black_box(i64::MIN) / -1;
    v as i128
}

fn main() {
    println!("{}", compute());
}
