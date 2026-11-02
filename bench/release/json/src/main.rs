use serde_json::{Value, json};

fn main() {
    let mut rng = rng::Rng::new(2);
    let mut items = Vec::new();
    for i in 0..60_000 {
        items.push(json!({
            "id": i,
            "name": rng.word(),
            "score": rng.below(1_000_000) as f64 / 7.0,
            "tags": [rng.word(), rng.word(), rng.word()],
            "active": rng.below(2) == 1,
            "nested": { "x": rng.below(100), "y": [1, 2, 3] },
        }));
    }
    let doc = Value::Array(items);
    let mut total = 0usize;
    for _ in 0..4 {
        let text = serde_json::to_string(&doc).unwrap();
        let back: Value = serde_json::from_str(&text).unwrap();
        total += text.len();
        total += back.as_array().unwrap().iter().filter(|v| v["active"].as_bool() == Some(true)).count();
    }
    println!("{total}");
}
