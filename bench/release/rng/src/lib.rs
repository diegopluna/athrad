//! A fixed xorshift generator, so every workload sees the same inputs.

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    pub fn word(&mut self) -> String {
        let len = 3 + self.below(8) as usize;
        (0..len).map(|_| (b'a' + self.below(26) as u8) as char).collect()
    }
}
