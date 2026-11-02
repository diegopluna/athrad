#[derive(Clone, Copy)]
struct Body {
    p: [f64; 3],
    v: [f64; 3],
    m: f64,
}

fn energy(bodies: &[Body]) -> f64 {
    let mut e = 0.0;
    for (i, b) in bodies.iter().enumerate() {
        e += 0.5 * b.m * b.v.iter().map(|v| v * v).sum::<f64>();
        for c in &bodies[i + 1..] {
            let d: f64 = (0..3).map(|k| (b.p[k] - c.p[k]).powi(2)).sum::<f64>().sqrt();
            e -= b.m * c.m / d;
        }
    }
    e
}

fn step(bodies: &mut [Body], dt: f64) {
    let n = bodies.len();
    for i in 0..n {
        for j in i + 1..n {
            let d = [0, 1, 2].map(|k| bodies[i].p[k] - bodies[j].p[k]);
            let d2 = d.iter().map(|x| x * x).sum::<f64>();
            let mag = dt / (d2 * d2.sqrt());
            let (mi, mj) = (bodies[i].m, bodies[j].m);
            for k in 0..3 {
                bodies[i].v[k] -= d[k] * mj * mag;
                bodies[j].v[k] += d[k] * mi * mag;
            }
        }
    }
    for b in bodies.iter_mut() {
        for k in 0..3 {
            b.p[k] += dt * b.v[k];
        }
    }
}

fn main() {
    let mut rng = rng::Rng::new(7);
    let mut bodies: Vec<Body> = (0..20)
        .map(|_| Body {
            p: [0, 1, 2].map(|_| rng.below(2000) as f64 / 100.0 - 10.0),
            v: [0, 1, 2].map(|_| rng.below(200) as f64 / 1000.0 - 0.1),
            m: 1.0 + rng.below(100) as f64 / 10.0,
        })
        .collect();
    let before = energy(&bodies);
    for _ in 0..60_000 {
        step(&mut bodies, 0.001);
    }
    println!("{:.9} {:.9}", before, energy(&bodies));
}
