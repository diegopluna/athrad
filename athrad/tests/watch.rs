//! The host's watcher backend sees a file change, a save by rename, and
//! nothing when nothing happens.

#![feature(rustc_private)]

extern crate rustc_driver;

use std::time::Duration;

use athrad::watch::{OsWatcher, Watcher};

#[test]
fn sees_writes_and_renames() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("watch-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();

    let mut w = OsWatcher::new().unwrap();
    w.watch(&dir).unwrap();
    // Let the backend settle; some report the watch's own setup.
    let _ = w.wait(Some(Duration::from_millis(200))).unwrap();

    std::fs::write(&file, "fn main() { println!(); }\n").unwrap();
    assert!(!w.wait(Some(Duration::from_secs(5))).unwrap().is_empty(), "no event for a write");

    // An editor's save: write a new file, rename it over the old one.
    let tmp = dir.join(".main.rs.swp");
    std::fs::write(&tmp, "fn main() { println!(\"x\"); }\n").unwrap();
    std::fs::rename(&tmp, &file).unwrap();
    assert!(!w.wait(Some(Duration::from_secs(5))).unwrap().is_empty(), "no event for a rename");

    // Drain, then quiet.
    while !w.wait(Some(Duration::from_millis(200))).unwrap().is_empty() {}
    assert!(w.wait(Some(Duration::from_millis(100))).unwrap().is_empty());
}
