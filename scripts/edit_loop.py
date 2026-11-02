#!/usr/bin/env python3
"""Milestone 2's number: one edit to one function, rebuilt, against `cargo build`.

For each program, the edit flips one literal inside `main` back and forth
(EDIT_FROM <-> EDIT_TO), and each tool rebuilds after every flip:

  cargo build     a `cargo new` project with the program as src/main.rs,
                  debug profile, incremental, warmed up first
  athrad          the `athrad` command, one process per build, its caches
                  (rustc's incremental state with Athrad's objects, and
                  the per-function code cache) on disk, warmed up first
  athrad watch    one warm `athrad watch` process; the time is from the
                  write of the file to the end of the build, as the
                  watcher reports it

After each build both binaries run, and their output must be the same.

Then the same for Cargo projects: `cargo build` against `athrad build`, an
edit to the binary crate and one to its library dependency.

usage: scripts/edit_loop.py [RUNS]   (needs a release build of athrad)
"""

import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ATHRAD = ROOT / "target" / "release" / "athrad"
RUNS = int(sys.argv[1]) if len(sys.argv) > 1 else 10

# (program, literal to flip, what to flip it to). Each literal is in `main`.
PROGRAMS = [
    ("add.rs", "add(2, 3)", "add(2, 4)"),
    ("heap.rs", "0..10", "0..11"),
    ("traits.rs", "twice(|x| x * k, 2)", "twice(|x| x * k, 3)"),
    ("big.rs", "let (t, s) = work3(8);", "let (t, s) = work3(9);"),
]


def env_clean():
    env = dict(os.environ)
    env.pop("RUSTC_BOOTSTRAP", None)
    env.pop("ATHRAD_TIME", None)
    return env


def timed(cmd, cwd):
    t = time.perf_counter()
    r = subprocess.run(cmd, cwd=cwd, capture_output=True, env=env_clean())
    ms = (time.perf_counter() - t) * 1e3
    if r.returncode != 0:
        sys.exit(f"{' '.join(map(str, cmd))} failed:\n{r.stderr.decode()}")
    return ms


def flip(path, a, b):
    s = path.read_text()
    if a in s:
        s = s.replace(a, b, 1)
    else:
        s = s.replace(b, a, 1)
    path.write_text(s)


def output(exe):
    r = subprocess.run([exe], capture_output=True, env=env_clean())
    return r.returncode, r.stdout, r.stderr


def main():
    if not ATHRAD.exists():
        sys.exit("build athrad first: cargo build --release -p athrad")
    print(f"{'program':10} {'cargo build':>12} {'athrad':>8} {'athrad watch':>13}   (median of {RUNS}, ms)")
    for name, a, b in PROGRAMS:
        src = ROOT / "oracle" / "programs" / name
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            # cargo's project
            proj = tmp / "cargo"
            subprocess.run(["cargo", "new", "-q", "--vcs", "none", "--name", "prog", str(proj)], check=True)
            shutil.copy(src, proj / "src" / "main.rs")
            timed(["cargo", "build", "-q"], proj)
            cargo_exe = proj / "target" / "debug" / "prog"
            # athrad's copy
            work = tmp / "athrad"
            work.mkdir()
            shutil.copy(src, work / "main.rs")
            athrad_exe = work / "prog"
            timed([ATHRAD, "main.rs", "-o", "prog"], work)

            cargo_ms, athrad_ms, watch_ms = [], [], []
            for _ in range(RUNS):
                flip(proj / "src" / "main.rs", a, b)
                cargo_ms.append(timed(["cargo", "build", "-q"], proj))
                flip(work / "main.rs", a, b)
                athrad_ms.append(timed([ATHRAD, "main.rs", "-o", "prog"], work))
                if output(cargo_exe) != output(athrad_exe):
                    sys.exit(f"{name}: athrad and cargo disagree after an edit")

            # One warm watcher. It prints "built ... in N ms" per build.
            w = subprocess.Popen(
                [ATHRAD, "watch", "main.rs", "-o", "prog"],
                cwd=work,
                stderr=subprocess.PIPE,
                env=env_clean(),
                text=True,
            )
            try:
                w.stderr.readline()  # the first build
                for _ in range(RUNS):
                    time.sleep(0.2)
                    t = time.perf_counter()
                    flip(work / "main.rs", a, b)
                    line = w.stderr.readline()
                    if "built" not in line:
                        sys.exit(f"{name}: athrad watch said: {line}")
                    watch_ms.append((time.perf_counter() - t) * 1e3)
                    flip(proj / "src" / "main.rs", a, b)
                    timed(["cargo", "build", "-q"], proj)
                    if output(cargo_exe) != output(athrad_exe):
                        sys.exit(f"{name}: athrad watch and cargo disagree after an edit")
            finally:
                w.kill()

            med = lambda xs: statistics.median(xs)
            print(f"{name:10} {med(cargo_ms):12.0f} {med(athrad_ms):8.0f} {med(watch_ms):13.0f}")


# Cargo projects: (name, files to copy in, file to edit, literal, flip).
# `big` is big.rs as a package's main.rs.
PROJECTS = [
    ("shapes: bin edit", "oracle/crates/shapes", "app/src/util.rs", "== {} ==", "== {} ==!"),
    ("shapes: lib edit", "oracle/crates/shapes", "geom/src/lib.rs", "w * h,", "w * h + 0.0,"),
    ("big (crate)", None, "src/main.rs", "let (t, s) = work3(8);", "let (t, s) = work3(9);"),
]


def projects():
    athrad = ATHRAD.with_name("athrad")
    print()
    print(f"{'project':18} {'cargo build':>12} {'athrad build':>13}   (median of {RUNS}, ms)")
    for label, src, file, a, b in PROJECTS:
        with tempfile.TemporaryDirectory() as tmp:
            proj = Path(tmp) / "p"
            if src:
                shutil.copytree(ROOT / src, proj, ignore=shutil.ignore_patterns("target"))
            else:
                subprocess.run(["cargo", "new", "-q", "--vcs", "none", "--name", "big", str(proj)], check=True)
                shutil.copy(ROOT / "oracle" / "programs" / "big.rs", proj / "src" / "main.rs")
            timed(["cargo", "build", "-q"], proj)
            timed([athrad, "build", "-q"], proj)
            cargo_ms, athrad_ms = [], []
            for _ in range(RUNS):
                flip(proj / file, a, b)
                time.sleep(0.2)
                cargo_ms.append(timed(["cargo", "build", "-q"], proj))
                time.sleep(0.2)
                athrad_ms.append(timed([athrad, "build", "-q"], proj))
            med = statistics.median
            print(f"{label:18} {med(cargo_ms):12.0f} {med(athrad_ms):13.0f}")


if __name__ == "__main__":
    main()
    projects()
