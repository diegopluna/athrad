use std::fs;
use std::io::{Read, Write};

fn main() {
    let dir = std::env::temp_dir().join(format!("athrad-oracle-files-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("note.txt");
    {
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "line one").unwrap();
        f.write_all(b"line two\n").unwrap();
    }
    let mut s = String::new();
    fs::File::open(&path).unwrap().read_to_string(&mut s).unwrap();
    print!("{s}");
    println!("{} bytes, {} lines", fs::metadata(&path).unwrap().len(), s.lines().count());
    fs::remove_dir_all(&dir).unwrap();
    println!("{}", path.exists());
    match fs::read_to_string(dir.join("missing")) {
        Ok(_) => println!("found"),
        Err(e) => println!("error kind {:?}", e.kind()),
    }
}
