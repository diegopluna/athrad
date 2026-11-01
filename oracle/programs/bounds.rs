fn get(xs: &[i32], i: usize) -> i32 {
    xs[i]
}

fn main() {
    let xs = [10, 20, 30];
    println!("{}", get(&xs, 2));
    let i = std::hint::black_box(5);
    println!("{}", get(&xs, i));
}
