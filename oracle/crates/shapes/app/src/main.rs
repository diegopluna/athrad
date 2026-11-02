mod report;
mod util;

use geom::{Shape, total_area};

fn main() {
    let shapes = vec![Shape::circle(2.0), Shape::rect(3.0, 4.0), Shape::square(1.5)];
    for s in &shapes {
        println!("{}", report::line(s));
    }
    println!("total {:.3}", total_area(&shapes));
    println!("{:?} {:?}", geom::largest(&[3, 9, 2]), geom::largest::<f64>(&[]));
    let boxed: Vec<Box<dyn Fn(f64) -> f64>> = vec![Box::new(|x| x * 2.0), Box::new(move |x| x + total_area(&shapes))];
    println!("{:.2}", boxed.iter().map(|f| f(1.0)).sum::<f64>());
    println!("{}", util::banner("done"));
}
