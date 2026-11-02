fn main() {
    let mut x: u8 = std::hint::black_box(250);
    let mut steps = 0;
    while steps < 10 {
        x += 1;
        steps += 1;
        println!("{x}");
    }
}
