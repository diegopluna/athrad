fn pick(v: &[String], i: usize) -> &str {
    &v[i]
}

fn main() {
    let words: Vec<String> = ["alpha", "beta"].iter().map(|s| s.to_string()).collect();
    let boxed = Box::new(words.clone());
    println!("{} {}", pick(&words, 1), boxed.len());
    let which = std::hint::black_box(3);
    if which == 3 {
        let r: Result<u8, String> = Err(format!("bad value {which}"));
        let n = r.unwrap_or(7);
        println!("{n}");
        let missing: Option<&String> = words.get(which);
        println!("{}", missing.map(|s| s.len()).unwrap_or(0));
        panic!("gave up on {} after {:?}", which, words);
    }
}
