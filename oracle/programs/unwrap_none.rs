fn find(xs: &[i32], want: i32) -> Option<usize> {
    xs.iter().position(|&x| x == want)
}

fn main() {
    let xs = vec![4, 8, 15, 16, 23, 42];
    println!("{:?}", find(&xs, 15));
    let at = find(&xs, std::hint::black_box(99)).expect("99 is not there");
    println!("{at}");
}
