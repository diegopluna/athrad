// Athrad's own frontend covers all of this program.
fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

fn collatz(mut n: u64) -> u32 {
    let mut steps = 0;
    while n != 1 {
        n = if n % 2 == 0 { n / 2 } else { 3 * n + 1 };
        steps += 1;
    }
    steps
}

fn is_prime(n: u32) -> bool {
    if n < 2 {
        return false;
    }
    let mut d = 2;
    while d * d <= n {
        if n % d == 0 {
            return false;
        }
        d += 1;
    }
    true
}

fn sum_to(n: i64) -> i64 {
    let mut s = 0;
    for i in 1..=n {
        s += i;
    }
    s
}

fn classify(x: i32) -> i32 {
    let r = if x < 0 {
        -1
    } else if x == 0 {
        0
    } else {
        1
    };
    r
}

fn main() {
    println!("gcd {} {}", gcd(48, 18), gcd(17, 5));
    println!("collatz(27) = {}", collatz(27));
    let mut primes = 0;
    for n in 0..100 {
        if is_prime(n) {
            primes += 1;
        }
    }
    println!("{primes} primes below 100");
    println!("{} {} {}", sum_to(100), sum_to(0), sum_to(-5));
    println!("{} {} {}", classify(-7), classify(0), classify(9));
    let mut i = 0u8;
    loop {
        i += 1;
        if i == 10 {
            break;
        }
        if i % 3 == 0 {
            continue;
        }
        print!("{i} ");
    }
    println!("{:?}", i);
    let big: u128 = 1 << 100;
    let neg: i128 = -170141183460469231731687303715884105728;
    println!("{big} {neg} {}", u128::MAX);
    println!("{} {} {}", 300i32 as u8, -1i64 as u32, (200u8 as i8) as i64);
    println!("{} {} {}", true && !false, 5 > 3 || false, true ^ true);
    println!("{} {} {}", 0xffu8 & 0x0f, 1u16 << 15, -16i32 >> 2);
    println!("{{literal}} {0} {0:?} {1}", 7, u8::BITS);
    eprintln!("to stderr {}", i64::MIN);
}
