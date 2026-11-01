struct Noisy(&'static str);

impl Drop for Noisy {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn make(name: &'static str) -> Noisy {
    Noisy(name)
}

fn main() {
    let _a = make("a");
    {
        let _b = make("b");
        println!("inner");
    }
    let c = make("c");
    drop(c);
    let v = vec![Noisy("v1"), Noisy("v2")];
    println!("len {}", v.len());
    let moved = Some(make("opt"));
    if let Some(n) = moved {
        println!("got {}", n.0);
    }
    println!("end");
}
