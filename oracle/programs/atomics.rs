use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

static COUNTER: AtomicUsize = AtomicUsize::new(0);
static NAME: OnceLock<String> = OnceLock::new();

fn bump() -> usize {
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

fn main() {
    for _ in 0..5 {
        bump();
    }
    println!("{}", COUNTER.load(Ordering::Relaxed));
    let a = AtomicI32::new(-5);
    println!("{} {} {}", a.fetch_max(3, Ordering::AcqRel), a.fetch_min(-9, Ordering::AcqRel), a.load(Ordering::Acquire));
    println!("{:?} {:?}", a.compare_exchange(-9, 7, Ordering::SeqCst, Ordering::Relaxed), a.compare_exchange(0, 1, Ordering::SeqCst, Ordering::Relaxed));
    let b = AtomicBool::new(false);
    println!("{} {} {}", b.swap(true, Ordering::SeqCst), b.fetch_xor(true, Ordering::SeqCst), b.load(Ordering::SeqCst));
    let u = AtomicU8::new(0xf0);
    println!("{} {} {} {}", u.fetch_and(0x3c, Ordering::SeqCst), u.fetch_or(1, Ordering::SeqCst), u.fetch_nand(0xff, Ordering::SeqCst), u.fetch_sub(1, Ordering::SeqCst));
    let mut x = 5;
    let p = AtomicPtr::new(&mut x);
    let mut y = 9;
    let old = p.swap(&mut y, Ordering::SeqCst);
    println!("{} {}", unsafe { *old }, unsafe { *p.load(Ordering::SeqCst) });
    println!("{}", NAME.get_or_init(|| "once".to_string()));
    println!("{}", NAME.get_or_init(|| "twice".to_string()));
    let shared = Arc::new(Mutex::new(vec![1, 2]));
    let other = Arc::clone(&shared);
    other.lock().unwrap().push(3);
    println!("{:?} {}", shared.lock().unwrap(), Arc::strong_count(&shared));
    drop(other);
    println!("{}", Arc::strong_count(&shared));
    let rw = RwLock::new(10);
    *rw.write().unwrap() += 1;
    println!("{}", *rw.read().unwrap());
    std::sync::atomic::fence(Ordering::SeqCst);
}
