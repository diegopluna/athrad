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

Milestones 0 through 4 are done on all three hosts: CI is green on Ubuntu (x86_64), macOS (aarch64), and Windows (x86_64, MSVC), and Linux aarch64 passes in Docker. The repository is public at https://github.com/diegopluna/athrad, so Actions minutes are free.

`athrad FILE.rs [-o OUT]` builds a one-file binary crate as `rustc --edition 2024 FILE.rs` would, and the oracle checks 17 programs that way: arithmetic and overflow panics, loops and recursion, structs, enums and `match`, `Option`/`Result`, `Vec`, `String`, `Box`, sorting, iterators and closures, generics, trait objects, `f32`/`f64` with their edge cases, `i128`/`u128`, bounds-check and `unwrap`/`expect`/`panic!` panics, `ExitCode`, a generated 1,800-line program, and a series of edits.

The pipeline:

```text
.rs ─rustc 1.98.1 (rustc-dev, in-process; Athrad is its codegen backend)─▶ MIR per codegen unit
    ─frontend─▶ ir ─verify, codegen (Cranelift)─▶ object per unit ─▶ cc / link.exe ─▶ exe
                                         std's rlibs and system libraries ─┘
```

* `rust-toolchain.toml` pins 1.98.1 with `rustc-dev`. `frontend` drives that `rustc` through `rustc_driver` (`#![feature(rustc_private)]`, which `.cargo/config.toml` allows on stable with `RUSTC_BOOTSTRAP=1` for this workspace only). `rustc` parses, type-checks, borrow-checks, and monomorphizes; Athrad lowers every mono item, writes the entry and allocator shims, and links against the real `std`. The oracle's `rustc` is the same compiler.
* `frontend` (a sixth crate): MIR → IR, after `rustc_codegen_cranelift` (the 1.98.1 source is the map). Every local lives in a stack slot; values move as scalars, scalar pairs, or addresses, as `rustc`'s layouts and calling conventions say. Blocks a constant `switchInt` rules out after monomorphization are traps, as `rustc` skips them.
* `ir`: integers `i8`–`i128`, stack slots, loads and stores, data with relocations, symbol addresses, direct and indirect calls with multiple results, overflow-checked arithmetic, bit operations, and floats. A float is the integer of its bits; float operations say it is one, and ABI parameters carry a float flag.
* `codegen`: Cranelift 0.137, `opt_level=none`, frame pointers, LLVM's `i128` ABI. It writes unwind tables itself: `.eh_frame` on ELF and Mach-O, `.pdata`/`.xdata` on COFF. `i128` division is inline, because compiler-builtins passes `i128` differently on each OS.
* `link`: unchanged seams. Upstream `raw-dylib` imports (Windows `std`) are already in the rlib. Under Cargo, `rustc` links instead (milestone 3).
* `oracle`: programs are only Rust source now, in `oracle/programs` (must agree) and `oracle/programs/later` (may be rejected, must never disagree). Thread ids in panic messages are masked, and both binaries run without `RUST_BACKTRACE`.

Still open. There is no debug info, so backtraces are empty. Windows has no landing pads, so a program whose destructor would run during a panic is rejected there. Inline assembly (except Windows `std`'s `__fastfail`), defining C-variadic functions, `f16`/`f128`, and 128-bit atomics are rejected on every host. The coverage section below is the list of what landed after the milestones, including landing pads on Linux and macOS.

### Milestone 1 timings

Whole-file builds, MacBook Pro (Apple Silicon), release `athrad`, median of 10, with the output binary written; `rustc` is the same 1.98.1 in debug. Athrad still runs `rustc`'s whole frontend every time; nothing is cached yet.

| Program | `athrad` | `rustc` |
|---|---|---|
| `add.rs` | 68 ms | 85 ms |
| `heap.rs` | 122 ms | 123 ms |
| `floats.rs` | 89 ms | 105 ms |
| `traits.rs` | 83 ms | 101 ms |

### Milestone 2: one edit, one function

Done, and green on all three hosts. An edit is rebuilt through two caches:

* **`rustc`'s incremental compilation, with Athrad as its codegen backend.** `frontend` wraps `rustc`'s own LLVM backend (so `cfg`s, target features, and the CPU are what `rustc` would use) and replaces its code generation: each codegen unit is lowered inside the unit's dep-graph task and becomes an object, which is registered as the unit's work product. A unit `rustc` marks green keeps its object and is not lowered at all. With incremental compilation on, Athrad asks for 16 units: `rustc`'s incremental default is a unit per module (67 for `heap.rs`), and each unit costs about a millisecond to check and link.
* **Machine code per function.** `codegen::Cache` keys each function by a SHA-256 of its IR (not its linkage, which `rustc`'s partitioning moves around), the kind and signature of each symbol it references, the ISA and its flags, and a hash of the `ir` and `codegen` sources. A hit skips lowering to Cranelift and that function's verification. Constant allocations are named by their contents, so editing one function does not rename the constants of the others. In a unit that changed, only functions whose IR changed are compiled: editing `main` in `heap.rs` compiles one function.

`athrad FILE.rs` keeps both caches next to the output, in `.athrad/STEM.cache` and `.athrad/STEM.incr/`. `athrad watch FILE.rs [-o OUT] [--run]` stays warm and rebuilds when the file's contents change. It uses the `Watcher` trait, backed by `notify` (`inotify`, `kqueue`, `ReadDirectoryChangesW`). It also asks the C compiler driver once for the linker command it would run, and runs that directly, which saves 11 ms per link on macOS. `ATHRAD_TIME=1` prints where a build's time went.

The oracle checks the edit loop itself: a program with `// edit: FROM => TO` lines (`oracle/programs/edits.rs`: a constant, a function body, a struct's layout, a string, a new enum use, an early return) is rebuilt after each edit with warm caches, and every build must agree with `rustc`, and the caches must have been used. That test caught a real bug on its first run: the unit's item list was read outside its dep-graph task.

**The number.** `scripts/edit_loop.py`: one literal in `main` flipped back and forth, median of 15, MacBook Pro (Apple Silicon), `cargo build` debug with incremental compilation, everything warm. `big.rs` is generated by `scripts/gen_big.py`: 1,800 lines, 7,373 functions after monomorphization.

| Program | `cargo build` | `athrad` | `athrad watch` |
|---|---|---|---|
| `add.rs` | 137 ms | 79 ms | 70 ms |
| `heap.rs` | 192 ms | 110 ms | 103 ms |
| `traits.rs` | 164 ms | 99 ms | 92 ms |
| `big.rs` | 267 ms | 176 ms | 158 ms |

Where an edit's time goes now (`heap.rs`, `athrad`): `rustc` 40 ms (parsing, incremental type checking and MIR, marking units green), lowering 5 ms, code generation 1 ms, linking 37 ms. For `big.rs`: `rustc` 91 ms, lowering 9 ms, code generation 3 ms, linking 45 ms. Athrad's own work is now a few milliseconds; the rest is `rustc`'s frontend and the system linker.

* Incremental compilation costs small programs about 15 ms over a whole rebuild with the function cache alone (`heap.rs` was 94 ms that way), and saves large ones much more (`big.rs` was 330 ms). It is always on in the `athrad` command; the oracle's plain builds run without it.
* On macOS the linker's floor is 19 ms (`ld` on an empty program) and the driver adds 11 ms. A faster linker is the next lever, and rule 4's profile now says so. `rust-lld` cannot link on macOS 27: it cannot read the SDK's `libSystem.tbd`. Linking `std` as a dylib saved only 4 ms, so Athrad does not.
* After the machine has been idle for a few hundred milliseconds, every build here (`cargo`'s, `rustc`'s, Athrad's) is slower: `heap.rs` in a warm `athrad watch` takes 66 ms back to back and about 85 ms with 400 ms between edits. The table is measured the way an edit happens, with pauses.

### Milestone 3: a crate, then one plain library dependency

Done, and green on all three hosts. `athrad build` and `athrad run` take Cargo's arguments and run `cargo build` / `cargo run` with Athrad compiling every crate, the package's and its dependencies', from crates.io too. Cargo runs `athrad-driver` as its `RUSTC_WRAPPER`, and the output goes to `target/athrad`, so it never mixes with `rustc`'s.

* **Two programs.** `athrad` is a small launcher that does not link the compiler, because `rustc_driver` takes 20 ms to load and a no-op `athrad build` should cost what `cargo build` does. `athrad-driver` holds the compiler, and serves both Cargo and the one-file commands.
* **`rustc` links.** Driven by Cargo, the backend hands its objects to `rustc` as compiled modules, and `rustc` links them the way it does LLVM's: rlibs with their metadata for libraries, and the executable with every link argument Cargo and the crates give. Only the `lib` (rlib) and `bin` crate types are accepted; a proc-macro crate, or any crate Athrad cannot compile, is a `rustc` error that names why. Build scripts are compiled like any binary and are not tested.
* **Caches per crate.** `rustc`'s incremental compilation works as in milestone 2 (Cargo passes `-C incremental` for workspace crates). The per-function code cache sits beside Cargo's incremental directory, one file per crate and `-C metadata`. Athrad's work products are filed under keys of their own (`athrad-o`, `athrad-facts`), so an incremental directory LLVM also wrote to never hands either backend the other's object.
* **Skipped cleanups across crates.** A type now counts as the standard library's only if its crate is in the sysroot; a dependency's `Drop` impls are the program's. Each crate leaves its trait-object facts (vtables and the trait-object drops its cleanups would skip) beside its rlib or executable, and the executable checks all of them together. Cargo builds a binary only after its dependencies' rlibs are complete, so the facts are there.
* **New in the IR's lowering:** `i128` multiplication overflow checks (`itoa` needs them).

The oracle builds each project in `oracle/crates` with `cargo build` and with `athrad build` and compares the runs. `shapes` is a workspace with modules and a path dependency; its `edits` file has a series of edits across both crates (a library function body, a format string in a submodule, a new enum variant used in both crates, a renamed function). `numbers` uses `itoa` from crates.io. The test lives in `athrad/tests/crates.rs`, where Cargo builds the `athrad` binaries first.

`scripts/edit_loop.py`, Cargo projects, median of 10 (same machine and method as above):

| Project | `cargo build` | `athrad build` |
|---|---|---|
| `shapes`, edit in the binary crate | 164 ms | 172 ms |
| `shapes`, edit in the library | 220 ms | 214 ms |
| `big.rs` as a package | 270 ms | 235 ms |

On a small project Athrad and `rustc` now cost the same, because both spend the time in the same places: Cargo, `rustc`'s frontend, and `rustc`'s link, which here is 45 ms of each edit. Athrad's own work is a few milliseconds. The one-file commands still link themselves and run `ld` without the C driver (11 ms less on macOS). Under Cargo, the way to do the same is to give `rustc` Athrad as its `-C linker`, running the command the driver would run; that, or `lld`, is the next lever.

### Milestone 4: a frontend of Athrad's own

Done, and green on all three hosts. The `subset` crate is a lexer, a parser, a type checker, and a lowering to Athrad IR, for one-file programs over integers and `bool`: `fn` items, `let` and `let mut`, arithmetic with the overflow and division checks of a debug build, comparisons, `if`, `while`, `loop`, `for` over integer ranges, `break`, `continue`, `return`, calls, `T::MAX`/`MIN`/`BITS`, `as` between integers, `std::hint::black_box`, and `print!`, `println!`, `eprint!`, `eprintln!` with `{}`, `{:?}`, `{0}`, and `{name}`. `athrad FILE.rs` and `athrad watch` try it first; Cargo builds do not use it.

**The rule, kept by falling back.** A program `subset` is not sure of goes to `rustc`, which compiles it or reports the error; `subset` never writes a diagnostic. It falls back on any syntax it does not know, and on everything in its subset that `rustc` rejects:

* type errors, unknown names, assigning to a binding that is not `mut`, wrong argument counts, unused format arguments;
* `overflowing_literals`, including `rustc`'s typing of a literal by the type it is cast to;
* `arithmetic_overflow` and `unconditional_panic`, the deny-by-default lints `rustc` fires when constant propagation shows an operation will panic. `subset` propagates constants at least as far as `rustc` does, through every local and into both arms of every `if`, so it rejects at least what `rustc` rejects. When in doubt it falls back.

`subset/src/lib.rs` has tests of programs that must fall back. Columns in panic locations are `rustc`'s display columns, so a file with tabs or non-ASCII text also falls back.

**`std` without `rustc`.** The programs `subset` compiles link with a runtime: a small Rust program (`athrad::runtime::SOURCE`) compiled when Athrad itself is built, and shipped inside the compiler. It provides the C `main`, `lang_start`, and the allocator shim; its `main` calls `athrad_user_main`, which `subset`'s object defines. It also provides `print!` helpers, so output goes through `std`'s own stdout and stderr. Building it also reads what `subset` needs to know about the pinned `std` from `rustc` (`frontend::StdAbi`): the symbols of `core`'s panic functions, and the layout of `core::panic::Location`. So an overflow panic calls the same `core` function `rustc`'s code calls, with the same location.

**The oracle.** Every program is built a second time with `subset` allowed, and `subset_*.rs` (control flow, 128-bit values, casts, printing, and a program for each of the ten panics), `add.rs`, `print_edges.rs`, and `overflow.rs` must not fall back. The first run caught a real difference: `rustc` builds `-170141183460469231731687303715884105728i128` as a constant, while `subset` negated at run time and panicked.

**The number.** `scripts/edit_loop.py`, same method as before:

| Program | `cargo build` | `athrad` | `athrad watch` |
|---|---|---|---|
| `add.rs` | 135 ms | 60 ms | 46 ms |
| `subset_flow.rs` | 148 ms | 60 ms | 47 ms |

`subset` parses, checks, and lowers either file in about 0.1 ms. What is left is the link (about 30–40 ms on macOS) and, for the command, starting the process.

The subset is deliberately small: the programs it covers are ones a dev loop rarely needs. Growing it means taking on `std`'s generic code (`Vec`, `String`, iterators, traits), which needs `std`'s MIR. That means `rustc`'s metadata, and with it most of `rustc`. The measurements say where the time is now, for programs `subset` does not cover: `rustc`'s frontend (40–90 ms) and the link (30–45 ms). The next levers are the linker (the C driver under Cargo, then `lld`) and, for multi-crate builds, starting dependents on early metadata as Headstart does.

### After the milestones: coverage

A survey built 35 popular crates without proc macros of their own (regex, memchr, hashbrown, indexmap, bytes, base64, semver, rand, parking_lot, crossbeam, syn through their dependencies, ...) with `athrad build`. That is 75 crates once their dependencies and build scripts are counted. At first 30 compiled. Now all 75 do, on macOS. `oracle/crates/popular` uses 24 of them together, threads and channels included; it prints exactly what `cargo build`'s binary prints, on macOS, and on Linux x86_64 and aarch64. What it took:

* **Landing pads (ELF and Mach-O).** An IR `TryCall` terminator becomes Cranelift's `try_call`. Each function's call sites go into an LSDA, and the function's CIE names `rust_eh_personality`: through a `DW.ref` cell on ELF, and through its GOT entry on Mach-O, where ld64 requires it. Destructors run during unwinding, `catch_unwind` works, and so do threads, which use it.
* **SIMD.** The `simd_*` intrinsics are lowered lane by lane in memory, and so are the LLVM intrinsics `core::arch` declares as foreign functions (NEON pairwise and across-lane operations, `tbl1`, `pmovmskb`, `pshufb`, barriers). This is slow and exact.
* **Atomics, thread-locals (Cranelift's TLS models per host), C-variadic calls** (std calls `fcntl`; Apple's arm64 ABI puts variadic arguments on the stack), **smart-pointer unsizing** (`Arc<T>` to `Arc<dyn Trait>`), `Location::caller`, wide-pointer comparisons.
* **Proc macros run only inside the compiler**, so the backend hands proc-macro crates to `rustc`'s own LLVM backend, which it wraps. Build scripts are compiled by Athrad.
* Nothing from an earlier build of the crate is reused. `-C incremental` is ignored, and every function is compiled again.

**The gap: Windows has no landing pads.** Rust on MSVC unwinds with SEH and C++-style funclets, which Cranelift cannot express. On Windows, Athrad still skips cleanups where that cannot be seen and rejects the rest, so it rejects anything with a `Drop` that a panic could run, `catch_unwind`, and threads. Programs and projects that need landing pads are marked `may-reject-on-windows`: there they may be rejected, never miscompiled. Closing the gap means building cleanups as SEH `__finally`-style handlers (`__C_specific_handler` scope tables in `.xdata`, with the cleanup code reachable from the handler), or catching in an `__except` filter, cleaning up in the frame, and raising the exception again. Both need design work before code. Until then, Windows accepts fewer programs than Linux and macOS.

Still rejected everywhere: inline assembly (except Windows `std`'s `__fastfail`), defining C-variadic functions, `f16`/`f128`, 128-bit atomics, `raw-dylib` in the crate being built, crate types other than `bin`, `lib`, and `proc-macro`.

### After coverage: the edit loop

Re-measured after landing pads and the rest of that work, and again with `cargo build --release` in the same run. `scripts/edit_loop.py`, median of 10, same machine and method as the tables above. One literal flipped back and forth, caches warm. One-file debug builds were run afterwards and matched `cargo build`'s binary every time. `popular` is `oracle/crates/popular`, one binary crate on 24 crates.io libraries.

`cargo build --release` is opt-level 3 and does not pass `-C incremental`, so an edit recompiles the whole crate it touched. Dependencies are already built. Athrad still compiles like a debug build, overflow checks included. The release column is rebuild time. The release binary is optimized.

| Program | `cargo build` | `cargo build --release` | `athrad` | `athrad watch` |
|---|---|---|---|---|
| `add.rs` | 138 ms | 124 ms | 58 ms | 46 ms |
| `subset_flow.rs` | 151 ms | 144 ms | 59 ms | 48 ms |
| `heap.rs` | 184 ms | 223 ms | 111 ms | 103 ms |
| `traits.rs` | 161 ms | 152 ms | 101 ms | 93 ms |
| `big.rs` | 268 ms | 1,683 ms | 182 ms | 168 ms |

| Project | `cargo build` | `cargo build --release` | `athrad build` |
|---|---|---|---|
| `shapes`, edit in the binary crate | 165 ms | 189 ms | 173 ms |
| `shapes`, edit in the library | 225 ms | 245 ms | 217 ms |
| `big.rs` as a package | 278 ms | 1,710 ms | 240 ms |
| `popular`, edit in the binary | 428 ms | 925 ms | 299 ms |

On a small crate, release is about as fast as `cargo build`, and on `add.rs` a little faster. Optimizing a few functions costs less than the debug rebuild's incremental bookkeeping. The cost shows up when the crate is large. `big.rs` is 1,800 lines and 7,373 functions, and release optimizes all of them on every edit: 1.7 s, about 10× `athrad watch`. `popular` reoptimizes one binary crate: 925 ms against 299 ms. `shapes` changes a small crate, so release stays near the debug rebuild.

The debug and Athrad columns sit within about 10 ms of the run from before release was added. `add.rs` and `subset_flow.rs` are still the subset frontend.

Where a warm one-file edit goes, inside the process (`ATHRAD_TIME=1`, the edited function already in the code cache):

| Program | `rustc` | lower | codegen | link | total |
|---|---|---|---|---|---|
| `add.rs` | — | 0.1 ms, its own frontend | 0.3 ms | 35 ms | 36 ms |
| `heap.rs` | 44 ms | 5 ms | 0.8 ms | 38 ms | 88 ms |
| `big.rs` | 97 ms | 9 ms | 4 ms | 42 ms | 152 ms |

The wall-clock table is that, plus starting the process (about 25 ms) or, for `athrad watch`, noticing the save (about 15 ms). The link is still the floor on a small file. On `heap.rs` and `big.rs`, `rustc`'s frontend is the larger piece.

That split is the warm function cache. The next section is the same programs with nothing kept from the previous build.

### The edit loop, with nothing kept

The per-function code cache, `rustc`'s incremental units, and the on-disk subset runtime are gone. `athrad watch` still stays up, so the process does not start again and the linker command is already known, and each build compiles the file again. Once a crate has eight functions they compile together, one thread per core, and each of those threads keeps one Cranelift context and clears it between functions. Cranelift uses its fast register allocator, and its own verifier is off: the IR was already checked, and the check does not change the machine code.

`rustc` walks the monomorphization graph on one thread, because a binary's only root is `main`. The queries that walk reads do not depend on the order of the walk, so Athrad evaluates them on the pool first. The memo lives for this compile. The walk still decides the item set, and a static is still visited there. Doing the same work by splitting the frontier for the pool to steal, instead of one wave per depth, measured the same 42 ms on `big.rs`: that time is the queries. Mangling and the size estimate run on the core that just scanned the body, which is when the partition would have asked. `scripts/edit_loop.py`, median of 10, same machine and method as the tables above.

| Program | `cargo build` | `cargo build --release` | `athrad` | `athrad watch` |
|---|---|---|---|---|
| `add.rs` | 127 ms | 126 ms | 61 ms | 45 ms |
| `subset_flow.rs` | 142 ms | 146 ms | 61 ms | 44 ms |
| `heap.rs` | 173 ms | 229 ms | 103 ms | 78 ms |
| `traits.rs` | 149 ms | 154 ms | 86 ms | 68 ms |
| `big.rs` | 265 ms | 1,710 ms | 310 ms | 265 ms |

| Project | `cargo build` | `cargo build --release` | `athrad build` |
|---|---|---|---|
| `shapes`, edit in the binary crate | 155 ms | 181 ms | 139 ms |
| `shapes`, edit in the library | 211 ms | 235 ms | 179 ms |
| `big.rs` as a package | 264 ms | 1,723 ms | 358 ms |
| `popular`, edit in the binary | 421 ms | 918 ms | 309 ms |

On a small file, compiling everything is cheaper than the bookkeeping that used to skip it. `heap.rs` in `athrad watch` is 78 ms, against 103 ms with the cache warm. `traits.rs` is 68 ms, against 93 ms. `shapes` moved the same way: 139 ms and 179 ms, against 173 ms and 217 ms.

`big.rs` matches `cargo build`. All 7,373 functions are compiled on every edit. `athrad watch` is 265 ms, and `cargo build` is 265 ms. The same watch was 350 ms when the monomorphization queries ran on one thread, and 168 ms when their machine code was kept. `cargo build --release` on that file is still 1.7 s. One-shot `athrad` is 310 ms because the process starts and the link goes through the C driver. `athrad watch` is already running and already resolved that driver down to `ld`. The package form is 358 ms, against 264 ms for `cargo build`: `athrad build` is started by Cargo and linked through that same C driver. `popular` is 309 ms, ahead of `cargo build` at 421 ms.

Where one build goes, inside the process (`ATHRAD_TIME=1`, the middle of 3 runs, nothing reused):

| Program | `rustc` | lower | codegen | link | total |
|---|---|---|---|---|---|
| `add.rs` | — | 0.0 ms, its own frontend | 0.5 ms (2 functions) | 36 ms | 37 ms |
| `heap.rs` | 32 ms | 4 ms | 5 ms (361) | 36 ms | 77 ms |
| `traits.rs` | 20 ms | 3 ms | 2 ms (90) | 37 ms | 61 ms |
| `big.rs` | 134 ms | 37 ms | 69 ms (7,373) | 43 ms | 283 ms |

The wall clock is that, plus starting the process, or for `athrad watch` noticing the save. On `big.rs` the 134 ms is `rustc`: the crate's analysis, and the monomorphization queries. After those queries the walk itself is a few milliseconds. Cranelift is 69 ms. The link is still the floor on a small file, about 36 ms.

### Release builds: as fast to run, faster to build

The goal changed: a release binary that runs as fast as `cargo build --release`'s, built in less time. Athrad's own code generator cannot meet the first half. On a CPU benchmark (sieve, collatz, `HashMap`, sort), LLVM's binary ran in 0.45 s; Athrad's best Cranelift settings (`opt_level=speed`, the backtracking register allocator, release MIR) ran in 2.97 s, and today's default in 9.9 s, 3x slower than `rustc`'s debug build. Every local lives in a stack slot and small functions are not inlined. So a release mode keeps LLVM generating the code, and only the build around it can get faster.

`bench/release` holds ten run-time workloads (regex, serde_json, `HashMap`, sorting, miniz_oxide, sha2, integer loops, n-body, `BTreeMap`, strings), each printing a checksum. `scripts/release_bench.py` builds them under `cargo build --release` and under candidate settings, times clean builds and a one-file edit, runs every binary interleaved across settings, and fails on any output that differs. First runs, on boat.dev sandboxes (AMD Ryzen 7 9700X, 4 vCPU), 3 builds and 9 runs per setting, ratios against cargo in the same sandbox:

| Setting | Clean build | Edit | Run geomean / worst |
|---|---|---|---|
| `-C lto=off` | 0.83x | 0.53x | 1.02x / 1.19x (btree) |
| `-Zthreads=4` | 0.90x | noise | 0.97x / 1.08x |
| `opt-level=2` | 1.19x | 1.07x | 1.00x / 1.12x (sort) |
| `-Zshare-generics=yes` | 1.03x | 0.65x | 1.02x / 1.08x (strings) |
| `codegen-units=256` | 1.00x | 0.89x | 0.99x / 1.04x |

The same cargo build took 5.8 s on one sandbox and 10.2 s on the other, so compare only within a run, and treat effects under about 10% as unresolved at 3 rounds. No `rustc` setting gives a large cut without slowing some binary: LLVM's optimizer is most of a release build, and it is the part that makes the code fast.
