use miniz_oxide::deflate::compress_to_vec;
use miniz_oxide::inflate::decompress_to_vec;

fn main() {
    let mut rng = rng::Rng::new(5);
    let mut data = Vec::new();
    while data.len() < 6_000_000 {
        data.extend_from_slice(rng.word().as_bytes());
        data.push(b' ');
        if rng.below(10) == 0 {
            data.extend((0..rng.below(64)).map(|_| rng.next() as u8));
        }
    }
    let mut total = 0usize;
    for level in [1, 6] {
        let packed = compress_to_vec(&data, level);
        let unpacked = decompress_to_vec(&packed).unwrap();
        assert_eq!(unpacked, data);
        total += packed.len();
    }
    println!("{total}");
}
