#[derive(Debug, Clone, Copy)]
enum Shape {
    Circle(u32),
    Square { side: u32 },
    Dot,
}

fn area(s: Shape) -> u64 {
    match s {
        Shape::Circle(r) => 3 * (r as u64) * (r as u64),
        Shape::Square { side } => (side as u64).pow(2),
        Shape::Dot => 0,
    }
}

fn first_even(xs: &[i32]) -> Option<i32> {
    for &x in xs {
        if x % 2 == 0 {
            return Some(x);
        }
    }
    None
}

fn parse(s: &str) -> Result<u8, String> {
    s.parse::<u8>().map_err(|e| format!("{s}: {e}"))
}

fn main() {
    for s in [Shape::Circle(2), Shape::Square { side: 5 }, Shape::Dot] {
        println!("{s:?} -> {}", area(s));
    }
    println!("{:?} {:?}", first_even(&[1, 3, 4, 6]), first_even(&[1, 3]));
    println!("{:?} {:?} {:?}", parse("42"), parse("300"), parse("x"));
    let opt: Option<Option<bool>> = Some(None);
    println!("{opt:?} {}", matches!(opt, Some(None)));
    let c = 'é';
    println!("{c} {} {}", c as u32, c.len_utf8());
}
