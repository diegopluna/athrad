#[derive(Debug, Clone, Copy, PartialEq)]
struct Point {
    x: i32,
    y: i32,
}

impl Point {
    fn manhattan(self, other: Point) -> u32 {
        self.x.abs_diff(other.x) + self.y.abs_diff(other.y)
    }
}

#[derive(Debug)]
struct Rect {
    min: Point,
    max: Point,
    label: &'static str,
}

fn main() {
    let a = Point { x: 1, y: -2 };
    let b = Point { x: -4, y: 7 };
    let r = Rect { min: a, max: b, label: "box" };
    println!("{:?}", a);
    println!("{r:?}");
    println!("{:#?}", r);
    println!("{}", a.manhattan(b));
    println!("{}", a == b);
    let (x, y) = (r.max.x, r.min.y);
    println!("{x} {y} {}", r.label.len());
}
