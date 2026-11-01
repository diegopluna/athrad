# Athrad

This workspace is empty on purpose. This file is the whole project so far. The first commit after this one is the compiler, not a port of anything else.

**Athrad** is Sindarin for a ford. Source stands on one bank, a running program on the other, and the result is the one `rustc` would have carried across. Linux, macOS, and Windows are the three shores. The command is `athrad`.

The name Narya was dropped. It is already a project on this machine.

## What we are building

A dev compiler for ordinary Rust. A program Athrad accepts must be valid Rust and must print the same output as `rustc`. When Athrad cannot match `rustc`, it rejects the program. Release builds and CI stay on `rustc`.

The speed that matters is the edit loop: change one function, rebuild, run. A faster clean build of a huge crate comes later.

The three host operating systems are in CI from the first commit. A backend that only exists on one OS is out, even if it is faster there.

## What was already tried, elsewhere

A GPUI window named Vilya lives in a different directory (`~/Developer/vilya`). It is `gpui` 0.2.2, edition 2024. `cargo run` there opens a window that says “Hello, Vilya!”. That build was on macOS Apple Silicon with `rustc` 1.98.1, and it needed the Xcode Metal toolchain. It is not part of this workspace, and it is not the first program Athrad has to compile. GPUI needs proc macros, `unsafe`, and a GPU backend.

Lust (`t3dotgg/lust-lang` 0.2.0, `next` branch) was run on that window’s `src/main.rs`. Lust is a subset compiler with a warm daemon, and it is fast on code it accepts. It refused the crate in about 9 seconds with **L0508**: `gpui` is not a Lust source crate, adapter, or shim. Lust ships adapters for 18 crates and does not support proc macros, `unsafe`, or `async`. The published binary is Linux x86_64 only. Do not start Athrad by porting Lust.

Three existing projects bound the design:

| Project | What it is | Why it is not this workspace |
|---|---|---|
| Cranelift (`rustc-codegen-cranelift`) | Debug codegen backend for real `rustc` | The code generator to reuse. Linux and macOS on x86_64 and aarch64, Windows on x86_64. Windows on ARM is missing. It does not make type-checking faster. |
| Blaze (`indexable-inc/blaze`) | Incremental compiler, `blaze` where you would type `cargo` | The right shape for the edit loop. v2 is macOS Apple Silicon and Linux. Windows has no v2 installer. Its plan rewrites the trait solver, borrow checker, and linker. |
| Headstart (`PowderworksCode/headstart`) | Patched `rustc` and Cargo that start dependents on early metadata | The reference if the win is overlapping crate compiles inside real `rustc`. Gains are tens of percent. You build that compiler yourself. It already runs everywhere `rustc` runs. |

## Rules

1. `rustc` is the oracle. Same source, same stdout, stderr, and exit status. A mismatch is an Athrad bug.
2. Athrad owns the dev loop only. It does not replace `rustc` for release or CI.
3. Linux, macOS, and Windows are in CI before the second language feature.
4. No new borrow checker, trait solver, or linker until the pipeline below is green and an edit of one function is measured against `cargo build`.
5. The first program is a function that adds two numbers and prints the result. GPUI, proc macros, build scripts, `async`, and `unsafe` stay out until a milestone below names them.

## The workspace to create here

A Cargo workspace with four crates, in this directory:

| Crate | Job |
|---|---|
| `ir` | A tiny instruction set we control: integers, calls, branches, returns. |
| `codegen` | Lower that IR through Cranelift. |
| `link` | Write an object file with the `object` crate (ELF, Mach-O, or COFF) and link it with the host C compiler. |
| `oracle` | Compile the same program with `rustc`, run both binaries, fail if they differ. |

CI on the first commit: Ubuntu, macOS, Windows.

Three interfaces exist from the first week, even when each has one implementation:

* Object format: ELF, Mach-O, COFF.
* Linker command: the host `cc` (and `lld` later, if a profile says the system linker is the slow part).
* File watcher: `inotify`, `kqueue`, `ReadDirectoryChangesW`. The watcher can wait until milestone 2. The trait should exist so the daemon does not grow a Linux-only core.

## Milestones

**0. The oracle and one program.** A function that adds two numbers and prints the result. Athrad’s binary matches `rustc` on all three hosts. This is the project existing.

**1. Rust in, same pipeline out.** Take the MIR of one function from `rustc` and feed it to `codegen`. `rustc_codegen_cranelift` is the map for that lowering. Still one file, no dependencies.

**2. One edit, one function.** Hash each function body. A change rebuilds that function and relinks. Measure it against `cargo build` on that file before adding a second file. This is the number that matters.

**3. A crate, then one plain library dependency.** Modules and `Cargo.toml`. Proc macros, build scripts, and `async` stay out.

**4. A frontend of our own, only for the subset the pipeline already runs.** Reject anything that would disagree with `rustc`. Headstart’s patches are the reference if, instead, the win we want is overlapping crate compiles inside real `rustc`.

## First week

1. Turn this directory into the workspace above, with a failing oracle test and CI for the three operating systems.
2. Make the add-and-print program pass on this Mac, then on Linux and Windows CI.
3. Record a baseline: clean `cargo build` and a one-line edit of that same file, with `cargo build --timings` (nightly). Write the numbers under this heading. Do not optimize before those numbers exist.

   **Baseline, recorded.** MacBook Pro, Apple Silicon, macOS 27, `rustc` 1.98.1, `scripts/baseline.sh` (10 runs each, wall time, `cargo build --timings`; `--timings` is stable now, nightly is not needed). Program: `oracle/programs/add.rs` as a fresh `cargo new` binary, debug profile.

   | Build | Median | Range |
   |---|---|---|
   | Clean `cargo build` (after `cargo clean`) | 0.309 s | 0.305–0.497 s |
   | One-line edit (`add(2, 3)` ↔ `add(2, 4)`), `cargo build` | 0.127 s | 0.126–0.134 s |
   | No-op `cargo build` | 0.037 s | 0.035–0.038 s |

   For scale, not comparison: Athrad’s back half on the same program (hand-written IR → verify → Cranelift → object → link) is 29 ms median: verify and codegen 0.23 ms, writing the object 0.11 ms, the link 28 ms. `cargo run --release -p oracle --example time_athrad` prints each phase.

   **Linker profile** (macOS, same machine). It started at 75 ms because `athrad::build` found the linker on every build: `link::HostCc::new` runs the `cc` crate’s compiler probes (48 ms warm, 70 ms in a fresh process) only to return `cc`. The caller now finds it once and passes it in, and on Unix hosts `HostCc` reads `CC_<target>`, `CC_<target_u>`, `HOST_CC`, `CC` itself, as the `cc` crate would, and falls back to `cc`: finding the linker is now free. The `cc` crate is still used on MSVC, where it finds `link.exe` and its environment, and for non-host targets. The 28 ms link through `cc` is about 11 ms for the `clang` driver, 8 ms for `ld` to start, and 10 ms of linking; calling `ld` directly takes 18 ms but means repeating the driver’s work (SDK, platform version, and on Linux the crt objects and dynamic loader) on three hosts, and `rustc` links through `cc` too. `rust-lld` from rustup (LLD 22.1.8) cannot link on macOS 27: it rejects the `arm64e.x1` architecture in the SDK’s `libSystem.tbd`.
4. Read, in this order, only enough to copy the seams: Cranelift’s object-emission API, the `object` crate’s write path for the host format, and the MIR-to-Cranelift lowering in `rustc_codegen_cranelift`. Blaze’s v2 plan is a map of later lanes, not the first month’s work.

## Out of scope until a milestone names it

* A parser, type checker, trait solver, or borrow checker.
* A linker. Wild is Linux-only. mold is Linux. The system linker and `lld` are enough.
* A C backend and TCC. That is Lust’s speed, and it is why Lust is stuck on one OS.
* Proc macros, build scripts, `async`, and `unsafe`.
* The Vilya window.

## State of this directory

Milestones 0 and 1 are done on all three hosts: CI is green on Ubuntu (x86_64), macOS (aarch64), and Windows (x86_64, MSVC), and Linux aarch64 passes in Docker. The repository is public at https://github.com/diegopluna/athrad, so Actions minutes are free.

`athrad FILE.rs [-o OUT]` builds a one-file binary crate as `rustc --edition 2024 FILE.rs` would, and the oracle checks 14 programs that way: arithmetic and overflow panics, loops and recursion, structs, enums and `match`, `Option`/`Result`, `Vec`, `String`, `Box`, sorting, iterators and closures, generics, trait objects, `f32`/`f64` with their edge cases, `i128`/`u128`, bounds-check and `unwrap`/`expect`/`panic!` panics, and `ExitCode`.

The pipeline:

```text
.rs ─rustc 1.98.1 (rustc-dev, in-process)─▶ MIR ─frontend─▶ ir ─verify─▶ codegen (Cranelift) ─▶ object ─▶ cc / link.exe ─▶ exe
                                                                          std's rlibs and system libraries ─┘
```

* `rust-toolchain.toml` pins 1.98.1 with `rustc-dev`. `frontend` drives that `rustc` through `rustc_driver` (`#![feature(rustc_private)]`, which `.cargo/config.toml` allows on stable with `RUSTC_BOOTSTRAP=1` for this workspace only). `rustc` parses, type-checks, borrow-checks, and monomorphizes; Athrad lowers every mono item, writes the entry and allocator shims, and links against the real `std`. The oracle's `rustc` is the same compiler.
* `frontend` (a sixth crate): MIR → IR, after `rustc_codegen_cranelift` (the 1.98.1 source is the map). Every local lives in a stack slot; values move as scalars, scalar pairs, or addresses, as `rustc`'s layouts and calling conventions say. Blocks a constant `switchInt` rules out after monomorphization are traps, as `rustc` skips them.
* `ir`: integers `i8`–`i128`, stack slots, loads and stores, data with relocations, symbol addresses, direct and indirect calls with multiple results, overflow-checked arithmetic, bit operations, and floats. A float is the integer of its bits; float operations say it is one, and ABI parameters carry a float flag.
* `codegen`: Cranelift 0.137, `opt_level=none`, frame pointers, LLVM's `i128` ABI. It writes unwind tables itself: `.eh_frame` on ELF and Mach-O, `.pdata`/`.xdata` on COFF. `i128` division is inline, because compiler-builtins passes `i128` differently on each OS.
* `link`: unchanged seams. Upstream `raw-dylib` imports (Windows `std`) are already in the rlib.
* `oracle`: programs are only Rust source now, in `oracle/programs` (must agree) and `oracle/programs/later` (may be rejected, must never disagree). Thread ids in panic messages are masked, and both binaries run without `RUST_BACKTRACE`.

Not yet, and what happens instead:

* **Landing pads.** A panic unwinds through Athrad's frames without running their cleanup. That is right only when nobody can tell, so Athrad rejects `catch_unwind` (and thread spawns, which use it): every panic then ends the process. It also rejects programs whose cleanup would run a destructor with an outside effect: the program's own `Drop` impls, `BufWriter`, and trait objects that may hold either. `oracle/programs/later/drops.rs` is the case. Real landing pads are Cranelift `try_call` plus an LSDA on ELF and Mach-O, as `rustc_codegen_cranelift` does; Windows needs SEH and has no answer yet, and Athrad must not accept on one OS what it rejects on another.
* **SIMD.** `HashMap` reaches `core::arch` (NEON on aarch64, SSE2 on x86_64) through hashbrown; `simd_*` and `llvm.*` intrinsics are unsupported. `oracle/programs/later/hashmap.rs` is the case.
* Thread-locals, atomics, inline and global assembly, `f16`/`f128`, `i128` multiplication overflow checks, C-variadic functions, and `link-arg`/local `raw-dylib` libraries are rejected with a message naming the construct.
* No debug info, so backtraces are empty.

### Milestone 1 timings

Whole-file builds, MacBook Pro (Apple Silicon), release `athrad`, median of 10, with the output binary written; `rustc` is the same 1.98.1 in debug. Athrad still runs `rustc`'s whole frontend every time; nothing is cached yet.

| Program | `athrad` | `rustc` |
|---|---|---|
| `add.rs` | 68 ms | 85 ms |
| `heap.rs` | 122 ms | 123 ms |
| `floats.rs` | 89 ms | 105 ms |
| `traits.rs` | 83 ms | 101 ms |

Next is milestone 2: hash each function body, rebuild and relink only what changed, and measure that edit against `cargo build`.
