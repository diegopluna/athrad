use std::sync::Arc;

use anyhow::{Context, Result, bail};
use base64::Engine;
use bitflags::bitflags;
use rand::{Rng, SeedableRng};
use unicode_segmentation::UnicodeSegmentation;

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Perm: u8 {
        const READ = 1;
        const WRITE = 2;
        const EXEC = 4;
    }
}

fn parse_port(s: &str) -> Result<u16> {
    let n: u32 = s.parse().with_context(|| format!("not a number: {s:?}"))?;
    if n > 65535 {
        bail!("port {n} out of range");
    }
    Ok(n as u16)
}

fn main() -> Result<()> {
    // Text: regex, aho-corasick, memchr.
    let re = regex::Regex::new(r"(?P<y>\d{4})-(?P<m>\d{2})-(?P<d>\d{2})")?;
    let text = "dates: 2024-01-15, 1999-12-31 and 2026-11-02.";
    for c in re.captures_iter(text) {
        println!("{}/{}/{}", &c["d"], &c["m"], &c["y"]);
    }
    println!("{}", re.replace_all(text, "$d.$m.$y"));
    let ac = aho_corasick::AhoCorasick::new(["apple", "maple", "snapple"])?;
    let hay = "Nobody likes maple in their apple flavored Snapple.";
    let found: Vec<_> = ac.find_iter(hay).map(|m| (m.pattern().as_usize(), m.start())).collect();
    println!("{found:?}");
    println!("{:?} {:?}", memchr::memchr(b'k', b"bookkeeper"), memchr::memrchr(b'e', b"bookkeeper"));
    println!("{}", memchr::memmem::find_iter(b"abcabcabc", b"bc").count());
    println!("{:?}", "héllo wörld 👋🏽!".graphemes(true).collect::<Vec<_>>());
    println!("{}", textwrap::fill("The quick brown fox jumps over the lazy dog, twice over.", 20));
    println!("{} {:.3}", strsim::levenshtein("kitten", "sitting"), strsim::jaro_winkler("martha", "marhta"));

    // Collections.
    let mut hm = hashbrown::HashMap::new();
    for w in text.split_whitespace() {
        *hm.entry(w.len()).or_insert(0) += 1;
    }
    let mut lens: Vec<_> = hm.into_iter().collect();
    lens.sort();
    println!("{lens:?}");
    let mut im = indexmap::IndexMap::new();
    for (i, w) in ["zeta", "alpha", "mu", "alpha"].iter().enumerate() {
        im.insert(*w, i);
    }
    println!("{im:?}");
    let mut sv: smallvec::SmallVec<[u32; 4]> = smallvec::smallvec![5, 3, 9];
    sv.extend([1, 7]);
    sv.sort();
    println!("{sv:?} spilled={}", sv.spilled());
    let mut av = arrayvec::ArrayVec::<u8, 3>::new();
    av.push(1);
    av.push(2);
    println!("{av:?} {:?}", av.try_push(3).and_then(|_| av.try_push(4)));

    // Bytes and encodings.
    let mut buf = bytes::BytesMut::with_capacity(16);
    buf.extend_from_slice(b"hello ");
    buf.extend_from_slice(b"bytes");
    let frozen = buf.freeze();
    let head = frozen.slice(0..5);
    println!("{:?} {:?}", head, frozen.len());
    let enc = base64::engine::general_purpose::STANDARD.encode(b"Athrad, a ford");
    println!("{enc} {}", String::from_utf8(base64::engine::general_purpose::STANDARD.decode(&enc)?)?);
    println!("{} {:?}", hex::encode([0xde, 0xad, 0xbe, 0xef]), hex::decode("cafe"));
    let mut ib = itoa::Buffer::new();
    let (mut r1, mut r2) = (ryu::Buffer::new(), ryu::Buffer::new());
    println!("{} {} {}", ib.format(-123456789i64), r1.format(1.0f64 / 3.0), r2.format(1e21f64));

    // Numbers, versions, flags, errors.
    println!("{} {:?}", num_traits::pow(3u64, 13), num_traits::checked_pow(10u8, 3));
    let v = semver::Version::parse("1.98.1-beta.2+build.5")?;
    let req = semver::VersionReq::parse(">=1.90, <2")?;
    println!("{v} {} {}", req.matches(&v), v.pre);
    let p = Perm::READ | Perm::EXEC;
    println!("{p:?} {} {:?}", p.contains(Perm::WRITE), p.complement());
    println!("{:?} {}", parse_port("8080")?, parse_port("99999").unwrap_err());
    println!("{:#}", parse_port("x").unwrap_err());

    // Randomness, seeded.
    let mut rng = fastrand::Rng::with_seed(7);
    println!("{:?}", (0..5).map(|_| rng.u32(..100)).collect::<Vec<_>>());
    let mut std_rng = rand::rngs::StdRng::seed_from_u64(42);
    println!("{} {:.4}", std_rng.random_range(0..1000), std_rng.random::<f64>());

    // Concurrency.
    static LAZY: once_cell::sync::Lazy<Vec<u8>> = once_cell::sync::Lazy::new(|| vec![1, 2, 3]);
    println!("{:?}", *LAZY);
    let counter = Arc::new(parking_lot::Mutex::new(0u64));
    let (tx, rx) = crossbeam_channel::bounded::<u64>(4);
    let workers: Vec<_> = (0..4)
        .map(|w| {
            let tx = tx.clone();
            let counter = Arc::clone(&counter);
            std::thread::spawn(move || {
                for i in 0..100 {
                    *counter.lock() += 1;
                    if i % 25 == 0 {
                        tx.send(w * 1000 + i).unwrap();
                    }
                }
            })
        })
        .collect();
    drop(tx);
    let mut got: Vec<u64> = rx.iter().collect();
    for h in workers {
        h.join().unwrap();
    }
    got.sort();
    println!("{} {:?}", *counter.lock(), got);
    Ok(())
}
