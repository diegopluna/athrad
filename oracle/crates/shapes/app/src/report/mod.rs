mod fmt;

use geom::Shape;

pub fn line(s: &Shape) -> String {
    format!("{:>8} {}", s.name(), fmt::area(s.area()))
}
