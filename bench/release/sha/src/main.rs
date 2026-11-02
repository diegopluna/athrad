use sha2::{Digest, Sha256, Sha512};

fn main() {
    let mut rng = rng::Rng::new(6);
    let data: Vec<u8> = (0..8_000_000).map(|_| rng.next() as u8).collect();
    let mut acc = [0u8; 32];
    for chunk in data.chunks(4096) {
        let mut h = Sha256::new();
        h.update(acc);
        h.update(chunk);
        acc.copy_from_slice(&h.finalize());
    }
    let big = Sha512::digest(&data);
    println!("{:02x?} {:02x?}", &acc[..8], &big[..8]);
}
