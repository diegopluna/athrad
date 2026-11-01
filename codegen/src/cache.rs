//! Machine code for each function, kept across builds.
//!
//! A function's key is a SHA-256 of everything its machine code depends on:
//!
//! * Athrad's own code generator (a hash of the `ir` and `codegen` sources,
//!   and the Cranelift version), and the target ISA with its flags;
//! * the function's IR, which is what milestone 2 calls its body, but not
//!   its linkage;
//! * for each symbol it calls or takes the address of: whether it is code
//!   or data, defined here or imported (which decides between a direct call
//!   and one through the GOT), and a callee's signature.
//!
//! Symbols are recorded by name, never by Cranelift's ids, which change
//! whenever a function is added. On a hit, the cached bytes, relocations and
//! unwind info are put into the object as they are; nothing is lowered.
//!
//! The cache is a file of length-prefixed records, read whole at the start
//! and appended to at the end. Entries that no longer match anything stay in
//! it; `Cache::compact` drops them.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use cranelift_codegen::binemit::Reloc;
use cranelift_codegen::ir::{KnownSymbol, LibCall};
use cranelift_codegen::isa::TargetIsa;
use cranelift_codegen::isa::unwind::UnwindInfo;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Identifies this build of the code generator.
const BUILD_ID: &str = env!("ATHRAD_CODEGEN_BUILD_ID");

pub type Key = [u8; 32];

/// One function's compiled code, with symbols by name.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub align: u64,
    pub code: Vec<u8>,
    pub relocs: Vec<CachedReloc>,
    pub unwind: Option<UnwindInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CachedReloc {
    pub offset: u32,
    pub kind: Reloc,
    pub target: Target,
    pub addend: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum Target {
    Func(String),
    Data(String),
    LibCall(LibCall),
    Known(KnownSymbol),
    /// An offset into the function itself.
    SelfOffset(u32),
}

/// Hit and miss counts for one build.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub hits: usize,
    pub misses: usize,
}

#[derive(Default)]
pub struct Cache {
    path: Option<PathBuf>,
    entries: HashMap<Key, Vec<u8>>,
    /// Keys added since loading, to append on save.
    added: Vec<Key>,
    /// Keys used by the latest build, for compaction.
    used: HashSet<Key>,
}

impl Cache {
    /// A cache that lives only as long as this value: a warm process's.
    pub fn in_memory() -> Self {
        Cache::default()
    }

    /// The cache in `path`, empty if the file does not exist yet. A file
    /// that does not parse (cut short by a crash, say) is ignored from the
    /// first bad record on.
    pub fn load(path: &Path) -> Result<Self> {
        let mut cache = Cache { path: Some(path.to_owned()), ..Cache::default() };
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(cache),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut rest = &bytes[..];
        while rest.len() >= 36 {
            let key: Key = rest[..32].try_into().expect("32 bytes");
            let len = u32::from_le_bytes(rest[32..36].try_into().expect("4 bytes")) as usize;
            if rest.len() < 36 + len {
                break;
            }
            cache.entries.insert(key, rest[36..36 + len].to_vec());
            rest = &rest[36 + len..];
        }
        Ok(cache)
    }

    /// Append what this build added. A no-op for an in-memory cache.
    pub fn save(&mut self) -> Result<()> {
        let Some(path) = &self.path else {
            self.added.clear();
            return Ok(());
        };
        if self.added.is_empty() {
            return Ok(());
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut out = Vec::new();
        for key in self.added.drain(..) {
            write_record(&mut out, &key, &self.entries[&key]);
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        file.write_all(&out).with_context(|| format!("writing {}", path.display()))
    }

    /// Keep only the entries the latest build used, and rewrite the file.
    pub fn compact(&mut self) -> Result<()> {
        self.entries.retain(|k, _| self.used.contains(k));
        self.added.clear();
        let Some(path) = &self.path else { return Ok(()) };
        let mut out = Vec::new();
        for (key, val) in &self.entries {
            write_record(&mut out, key, val);
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, out).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Start a build: forget which entries the last one used.
    pub(crate) fn begin(&mut self) {
        self.used.clear();
    }

    pub(crate) fn get(&mut self, key: &Key) -> Option<Entry> {
        let bytes = self.entries.get(key)?;
        let entry = postcard::from_bytes(bytes).ok()?;
        self.used.insert(*key);
        Some(entry)
    }

    pub(crate) fn insert(&mut self, key: Key, entry: &Entry) -> Result<()> {
        let bytes = postcard::to_allocvec(entry).context("encoding a cache entry")?;
        if self.entries.insert(key, bytes).is_none() {
            self.added.push(key);
        }
        self.used.insert(key);
        Ok(())
    }
}

fn write_record(out: &mut Vec<u8>, key: &Key, val: &[u8]) {
    out.extend_from_slice(key);
    out.extend_from_slice(&(val.len() as u32).to_le_bytes());
    out.extend_from_slice(val);
}

/// What a referenced symbol is, as far as code generation can tell.
#[derive(Hash)]
pub(crate) enum SymbolKind<'a> {
    Func {
        import: bool,
        sig: &'a ir::Signature,
    },
    Data {
        import: bool,
    },
    /// Not in the module: verification rejects the function.
    Missing,
}

/// The key of `func`'s machine code. `symbol` describes a symbol by name.
pub(crate) fn key<'a>(isa: &dyn TargetIsa, func: &'a ir::Function, symbol: impl Fn(&str) -> SymbolKind<'a>) -> Key {
    let mut h = ShaHasher(Vec::with_capacity(4096));
    BUILD_ID.hash(&mut h);
    isa.triple().to_string().hash(&mut h);
    isa.flags().to_string().hash(&mut h);
    for f in isa.isa_flags() {
        f.to_string().hash(&mut h);
    }
    // Not its linkage: that is the symbol's, set when it is declared, and
    // rustc's partitioning changes it as other functions come and go.
    let ir::Function { name, linkage: _, sig, values, slots, blocks } = func;
    (name, sig, values, slots, blocks).hash(&mut h);
    let mut names: Vec<&str> = func
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .filter_map(|i| match &i.op {
            ir::Op::Call(name, _) | ir::Op::SymbolAddr(name) => Some(name.as_str()),
            _ => None,
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    for name in names {
        name.hash(&mut h);
        symbol(name).hash(&mut h);
    }
    Sha256::digest(&h.0).into()
}

/// A `std::hash::Hasher` that collects what derived `Hash` impls write,
/// for one SHA-256 over all of it: a key that is the same in every process.
/// (Feeding SHA-256 the many small writes one by one is slower.)
struct ShaHasher(Vec<u8>);

impl Hasher for ShaHasher {
    fn write(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    fn finish(&self) -> u64 {
        unreachable!("keys come from `finalize`")
    }
}
