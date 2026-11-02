use regex::Regex;
use std::hint::black_box;

fn main() {
    let mut rng = rng::Rng::new(1);
    let mut text = String::new();
    for i in 0..400_000 {
        text.push_str(&rng.word());
        text.push(if i % 17 == 0 { '\n' } else { ' ' });
        if i % 97 == 0 {
            text.push_str(&format!("user{}@host{}.example.com ", rng.below(1000), rng.below(50)));
        }
        if i % 131 == 0 {
            text.push_str(&format!("2024-{:02}-{:02} ", 1 + rng.below(12), 1 + rng.below(28)));
        }
    }
    let patterns = [
        r"[a-z0-9]+@[a-z0-9]+\.example\.com",
        r"\d{4}-\d{2}-\d{2}",
        r"\b[aeiou][a-z]{5,}\b",
        r"(?m)^[a-z]+ [a-z]+$",
        r"qu[a-z]*|z[a-z]{2}y",
    ];
    let mut total = 0usize;
    for _ in 0..3 {
        for p in patterns {
            let re = Regex::new(black_box(p)).unwrap();
            total += re.find_iter(&text).count();
            total += re.captures_iter(&text).take(1000).map(|c| c[0].len()).sum::<usize>();
        }
    }
    println!("{total}");
}
