// The oracle builds this program, then makes each edit below in turn and
// rebuilds with Athrad's caches warm (rustc's incremental state, the
// per-function code cache), checking every build against rustc.
//
// edit: let base = 10; => let base = 11;
// edit: a.wrapping_mul(31) => a.wrapping_mul(37)
// edit: struct Pair { left: u32, right: u32 } => struct Pair { left: u32, right: u32, extra: u64 } ;; Pair { left: i, right: i * i } => Pair { left: i, right: i * i, extra: 7 }
// edit: "pair {} {}" => "pair {}/{}"
// edit: let label = "sum"; => let label = "total";
// edit: shapes.push(Shape::Square(3)); => shapes.push(Shape::Square(3)); shapes.push(Shape::Circle(1));
// edit: fn describe(n: u64) -> String { => fn describe(n: u64) -> String { if n == 0 { return "none".into(); }
// edit: let base = 11; => let base = 0;

#[derive(Debug, Clone)]
struct Pair { left: u32, right: u32 }

#[derive(Debug)]
enum Shape {
    Circle(u32),
    Square(u32),
}

fn hash(a: u64) -> u64 {
    a.wrapping_mul(31) ^ (a >> 3)
}

fn describe(n: u64) -> String {
    format!("{n} ({:x})", hash(n))
}

fn pairs(n: u32) -> Vec<Pair> {
    (0..n).map(|i| Pair { left: i, right: i * i }).collect()
}

fn area(s: &Shape) -> u32 {
    match s {
        Shape::Circle(r) => 3 * r * r,
        Shape::Square(a) => a * a,
    }
}

fn main() {
    let base = 10;
    let label = "sum";
    let ps = pairs(4);
    for p in &ps {
        println!("pair {} {}", p.left, p.right);
    }
    println!("{:?}", ps.last());
    let total: u64 = (0..base).map(hash).sum();
    println!("{label} {total}");
    println!("{}", describe(base));
    let mut shapes = vec![Shape::Circle(2)];
    shapes.push(Shape::Square(3));
    println!("{:?} {}", shapes, shapes.iter().map(area).sum::<u32>());
    println!("{}", std::mem::size_of::<Pair>());
}
