use std::cell::{Cell, RefCell};

thread_local! {
    static COUNT: Cell<u32> = const { Cell::new(0) };
    static LOG: RefCell<Vec<String>> = RefCell::new(Vec::new());
}

fn note(s: &str) {
    COUNT.set(COUNT.get() + 1);
    LOG.with_borrow_mut(|l| l.push(format!("{}:{s}", COUNT.get())));
}

fn main() {
    note("a");
    note("b");
    note("c");
    LOG.with_borrow(|l| println!("{l:?}"));
    println!("{}", COUNT.get());
    let h: std::collections::HashMap<u8, u8> = (0..10).map(|i| (i, i * i)).collect();
    let mut keys: Vec<_> = h.into_iter().filter(|(_, v)| v % 2 == 1).collect();
    keys.sort();
    println!("{keys:?}");
}
