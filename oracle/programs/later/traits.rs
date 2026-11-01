trait Animal {
    fn name(&self) -> String;
    fn speak(&self) -> String {
        format!("{} makes a sound", self.name())
    }
}

struct Dog;
struct Cat {
    lives: u8,
}

impl Animal for Dog {
    fn name(&self) -> String {
        "Dog".into()
    }
    fn speak(&self) -> String {
        "Woof".into()
    }
}

impl Animal for Cat {
    fn name(&self) -> String {
        format!("Cat with {} lives", self.lives)
    }
}

fn twice<F: Fn(i32) -> i32>(f: F, x: i32) -> i32 {
    f(f(x))
}

fn apply(f: &dyn Fn(i32) -> i32, x: i32) -> i32 {
    f(x)
}

fn largest<T: PartialOrd + Copy>(xs: &[T]) -> T {
    let mut best = xs[0];
    for &x in xs {
        if x > best {
            best = x;
        }
    }
    best
}

fn main() {
    let zoo: Vec<Box<dyn Animal>> = vec![Box::new(Dog), Box::new(Cat { lives: 9 })];
    for a in &zoo {
        println!("{}", a.speak());
    }
    let k = 3;
    println!("{}", twice(|x| x * k, 2));
    println!("{}", apply(&|x| x - k, 10));
    let mut counter = 0;
    let mut inc = || counter += 1;
    inc();
    inc();
    println!("{counter}");
    println!("{} {} {}", largest(&[3, 9, 2]), largest(&[1.5, -2.0]), largest(&['a', 'z', 'q']));
}
