fn main() {
    let a: u128 = std::hint::black_box(340282366920938463463374607431768211455);
    let b: u128 = std::hint::black_box(18446744073709551629);
    println!("{} {} {}", a / b, a % b, a / 1);
    let c: i128 = std::hint::black_box(-170141183460469231731687303715884105727);
    let d: i128 = std::hint::black_box(-98765432109876543210);
    println!("{} {} {} {}", c / d, c % d, c / 3, c % -7);
    println!("{} {}", c.checked_add(-5).is_none(), (c - 1).wrapping_sub(1));
    println!("{:?} {:?}", 5u128.checked_sub(6), a.checked_add(1));
    let m: u128 = std::hint::black_box(0xffff_ffff_ffff_ffff_1234);
    for k in [1u128, 2, 3, 1 << 63, 1 << 64, 1 << 70, u128::MAX] {
        println!("u {:?} {:?}", m.checked_mul(k), k.checked_mul(k));
    }
    let n: i128 = std::hint::black_box(-0x7fff_ffff_ffff_ffff_ffff);
    for k in [1i128, -1, 2, -2, 1 << 40, -(1 << 41), i128::MAX, i128::MIN, 1 << 64, -(1 << 63)] {
        println!("i {:?} {:?} {:?}", n.checked_mul(k), k.checked_mul(-k), i128::MIN.checked_mul(k));
    }
    println!("{:?} {:?}", (1i128 << 63).checked_mul(-(1 << 64)), (-(1i128 << 63)).checked_mul(1 << 64));
    let e = std::hint::black_box(i128::MAX);
    println!("{}", e + 1);
}
