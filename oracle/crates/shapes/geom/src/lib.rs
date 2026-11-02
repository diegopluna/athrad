#[derive(Debug, Clone)]
pub enum Shape {
    Circle { r: f64 },
    Rect { w: f64, h: f64 },
}

impl Shape {
    pub fn circle(r: f64) -> Self { Shape::Circle { r } }
    pub fn rect(w: f64, h: f64) -> Self { Shape::Rect { w, h } }
    pub fn square(s: f64) -> Self { Self::rect(s, s) }

    pub fn area(&self) -> f64 {
        match self {
            Shape::Circle { r } => std::f64::consts::PI * r * r,
            Shape::Rect { w, h } => w * h,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Shape::Circle { .. } => "circle",
            Shape::Rect { w, h } if w == h => "square",
            Shape::Rect { .. } => "rect",
        }
    }
}

pub fn total_area(shapes: &[Shape]) -> f64 {
    shapes.iter().map(Shape::area).sum()
}

pub fn largest<T: PartialOrd + Copy>(xs: &[T]) -> Option<T> {
    xs.iter().copied().fold(None, |m, x| match m { Some(m) if m >= x => Some(m), _ => Some(x) })
}
