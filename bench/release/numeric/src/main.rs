use std::hint::black_box;

fn sieve(n: usize) -> usize {
    let mut is = vec![true; n + 1];
    is[0] = false;
    is[1] = false;
    let mut i = 2;
    while i * i <= n {
        if is[i] {
            let mut j = i * i;
            while j <= n {
                is[j] = false;
                j += i;
            }
        }
        i += 1;
    }
    is.iter().filter(|&&b| b).count()
}

fn collatz(limit: u64) -> (u64, u32) {
    let mut best = (0, 0);
    for start in 1..limit {
        let mut n = start;
        let mut steps = 0u32;
        while n != 1 {
            n = if n % 2 == 0 { n / 2 } else { 3 * n + 1 };
            steps += 1;
        }
        if steps > best.1 {
            best = (start, steps);
        }
    }
    best
}

fn matmul(n: usize) -> u64 {
    let a: Vec<u64> = (0..n * n).map(|i| (i % 7) as u64).collect();
    let b: Vec<u64> = (0..n * n).map(|i| (i % 5) as u64).collect();
    let mut c = vec![0u64; n * n];
    for i in 0..n {
        for k in 0..n {
            let x = a[i * n + k];
            for j in 0..n {
                c[i * n + j] = c[i * n + j].wrapping_add(x * b[k * n + j]);
            }
        }
    }
    c.iter().fold(0, |s, &x| s.wrapping_add(x))
}

fn main() {
    println!("{}", sieve(black_box(20_000_000)));
    println!("{:?}", collatz(black_box(1_500_000)));
    println!("{}", matmul(black_box(300)));
}
