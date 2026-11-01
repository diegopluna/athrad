fn main() {
    let a: u8 = 200;
    let b: i8 = -100;
    println!("{} {} {}", a.wrapping_add(100), b.wrapping_sub(100), a.checked_add(100).is_none());
    println!("{} {}", (a as i8), (b as u8));
    let x: i128 = -170141183460469231731687303715884105728;
    let y: u128 = 340282366920938463463374607431768211455;
    println!("{x} {y} {}", y / 7);
    println!("{} {} {}", 17i32 / -5, 17i32 % -5, (-17i32).rem_euclid(5));
    println!("{} {} {}", 1u64 << 40, -1i64 >> 3, 0xF0u8.count_ones());
    println!("{} {}", u32::MAX.leading_zeros(), 12u16.trailing_zeros());
    println!("{:x} {:#b} {:08.3}", 255, 5, 3.14159f64);
    let f = 2.5f32 * 4.0;
    println!("{f} {} {}", (7.9f64) as i32, (-1.0f64).sqrt().is_nan());
    let t = (1u8, 2u16, 3u32, 4u64, true, 'x');
    println!("{t:?}");
}
