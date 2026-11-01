fn main() {
    let a: u128 = std::hint::black_box(340282366920938463463374607431768211455);
    let b: u128 = std::hint::black_box(18446744073709551629);
    println!("{} {} {}", a / b, a % b, a / 1);
    let c: i128 = std::hint::black_box(-170141183460469231731687303715884105727);
    let d: i128 = std::hint::black_box(-98765432109876543210);
    println!("{} {} {} {}", c / d, c % d, c / 3, c % -7);
    println!("{} {}", c.checked_add(-5).is_none(), (c - 1).wrapping_sub(1));
    println!("{:?} {:?}", 5u128.checked_sub(6), a.checked_add(1));
    let e = std::hint::black_box(i128::MAX);
    println!("{}", e + 1);
}
