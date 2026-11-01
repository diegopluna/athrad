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

   For scale, not comparison: Athrad’s back half on the same program (hand-written IR → verify → Cranelift → object → `cc`) is 74 ms median, of which verify and codegen are 0.26 ms. The rest is the system linker. `cargo run --release -p oracle --example time_athrad`. Nothing has been optimized.
4. Read, in this order, only enough to copy the seams: Cranelift’s object-emission API, the `object` crate’s write path for the host format, and the MIR-to-Cranelift lowering in `rustc_codegen_cranelift`. Blaze’s v2 plan is a map of later lanes, not the first month’s work.

## Out of scope until a milestone names it

* A parser, type checker, trait solver, or borrow checker.
* A linker. Wild is Linux-only. mold is Linux. The system linker and `lld` are enough.
* A C backend and TCC. That is Lust’s speed, and it is why Lust is stuck on one OS.
* Proc macros, build scripts, `async`, and `unsafe`.
* The Vilya window.

## State of this directory

Milestone 0 is done on macOS (aarch64) and Linux (aarch64 and x86_64, checked in Docker). Windows is written but has not run yet: there is no Git remote, so CI has never run. Push to GitHub and watch `.github/workflows/ci.yml` before starting milestone 1.

* `ir`: types (`i8`, `i32`, `i64`), constants, wrapping arithmetic, compares, extends and truncates, calls by name, jumps, branches, returns. SSA with block parameters. `ir::verify` checks types, dominance, and terminators; codegen assumes verified IR.
* `codegen`: IR → Cranelift 0.137 → `object::write::Object` for the host. `opt_level=none`, PIC except on Windows.
* `link`: the `ObjectFormat` seam (ELF, Mach-O, COFF) and the `Linker` seam with one implementation, `HostCc`: `cc` on Unix, `link.exe` found by the `cc` crate on MSVC (`msvcrt`, `vcruntime`, `ucrt`, `kernel32`).
* `oracle`: builds each program in `oracle::programs::ALL` with `rustc --edition 2024` (debug) and with Athrad, runs both, and compares stdout, stderr, and exit status. `cargo test` runs it. One test checks that the oracle catches a wrong answer.
* `athrad` (a fifth crate, not in the plan above): the pipeline that joins the four (`athrad::build`), the runtime written in IR (`athrad::rt`: a C `main` that calls the program’s main, as `lang_start` does, and `println!` of an `i64` through `putchar`), and the `Watcher` trait with no implementation yet. It becomes the `athrad` command when there is Rust to take in, at milestone 1.

Things to know before milestone 1:

* There is no frontend. Each oracle program is Rust source plus the same program written by hand as IR.
* IR arithmetic wraps. `rustc` debug builds panic on overflow with exit code 101. The IR from MIR must carry MIR’s overflow assertions and reproduce the panic message, or reject the program.
* On Windows, the runtime puts stdout in binary mode (`_setmode(1, _O_BINARY)`), because Rust prints `\n` and the C runtime would print `\r\n`.
* Stdout goes through C `stdio`. Rust’s stdout is line-buffered and C’s is fully buffered when piped. That does not matter until a program writes to both stdout and stderr, or panics after printing.
* First-week step 4, reading Cranelift, `object`, and `rustc_codegen_cranelift`, has only gone as far as the object-emission API this needed.
