fn fib(n: u32) -> u64 {
    if n < 2 { n as u64 } else { fib(n - 1) + fib(n - 2) }
}

fn main() {
    let mut i = 0;
    while i < 20 {
        print!("{} ", fib(i));
        i += 1;
    }
    println!();
    for n in [25u32, 30] {
        println!("fib({n}) = {}", fib(n));
    }
}
