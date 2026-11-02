// athrad: may-reject-on-windows (no landing pads there yet)
struct Noisy(&'static str);

impl Drop for Noisy {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn inner(n: usize) -> usize {
    let _a = Noisy("inner");
    let v = vec![Noisy("v1"), Noisy("v2")];
    let i = std::hint::black_box(n);
    v.len() + [1, 2, 3][i]
}

fn main() {
    let _outer = Noisy("outer");
    let r = std::panic::catch_unwind(|| {
        let _c = Noisy("in closure");
        inner(7)
    });
    println!("caught: {}", r.is_err());
    let h = std::thread::spawn(|| {
        let _t = Noisy("thread");
        panic!("in thread");
    });
    println!("joined: {}", h.join().is_err());
    let _last = Noisy("last");
    inner(5);
}
