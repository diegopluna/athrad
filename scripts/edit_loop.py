#!/usr/bin/env python3
"""One edit to one function, rebuilt, against `cargo build`.

For each program, the edit flips one literal inside `main` back and forth
(EDIT_FROM <-> EDIT_TO), and each tool rebuilds after every flip:

  cargo build     a `cargo new` project with the program as src/main.rs,
                  debug profile, incremental, warmed up first
  cargo release   the same project, `cargo build --release`: opt-level 3,
                  and no incremental cache, which is what that command does
  athrad          the `athrad` command, one process per build. Nothing is
                  kept between builds: the crate is compiled again.
  athrad watch    one warm `athrad watch` process; the time is from the
                  write of the file to the end of the build, as the
                  watcher reports it. The process stays up so it does not
                  start again, and so the linker command is already known.

After each build both binaries run, and their output must be the same.

Then the same for Cargo projects: `cargo build` against `athrad build`, an
edit to the binary crate and one to its library dependency.

usage: scripts/edit_loop.py [RUNS]   (needs a release build of athrad)

Each run's samples are written to ATHRAD_EDIT_LOOP_JSON, or
/tmp/athrad_edit_loop.json.
"""

import json
import os
import select
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ATHRAD = ROOT / "target" / "release" / "athrad"
RUNS = int(sys.argv[1]) if len(sys.argv) > 1 else 10
SAMPLES = Path(os.environ.get("ATHRAD_EDIT_LOOP_JSON", "/tmp/athrad_edit_loop.json"))
RESULTS = {"runs": RUNS, "programs": [], "projects": []}

# (program, literal to flip, what to flip it to). Each literal is in `main`.
PROGRAMS = [
    ("add.rs", "add(2, 3)", "add(2, 4)"),
    ("subset_flow.rs", "sum_to(100)", "sum_to(101)"),
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


def save():
    SAMPLES.write_text(json.dumps(RESULTS))


def read_built(proc, name, timeout=120):
    """Read stderr until a line says the build finished.

    `rustc` may print warnings before that line. Waiting on the first line
    would take a warning for the build, and the real line would be counted
    against the next edit.
    """
    fd = proc.stderr.fileno()
    pending = b""
    end = time.perf_counter() + timeout
    while time.perf_counter() < end:
        if proc.poll() is not None and not select.select([fd], [], [], 0)[0]:
            sys.exit(f"{name}: athrad watch exited: {pending.decode(errors='replace')}")
        left = end - time.perf_counter()
        if not select.select([fd], [], [], min(0.5, max(left, 0)))[0]:
            continue
        chunk = os.read(fd, 65536)
        if not chunk:
            sys.exit(f"{name}: athrad watch closed its output: {pending.decode(errors='replace')}")
        pending += chunk
        while b"\n" in pending:
            line, pending = pending.split(b"\n", 1)
            if b"built" in line:
                return
            if line.startswith(b"athrad: error") or line.startswith(b"error"):
                sys.exit(f"{name}: athrad watch said: {line.decode(errors='replace')}")
    sys.exit(f"{name}: athrad watch produced no build within {timeout:.0f}s")


def main():
    if not ATHRAD.exists():
        sys.exit("build athrad first: cargo build --release -p athrad")
    print(
        f"{'program':15} {'cargo build':>12} {'release':>8} {'athrad':>8} {'athrad watch':>13}   (median of {RUNS}, ms)",
        flush=True,
    )
    for name, a, b in PROGRAMS:
        print(f"measuring {name}", file=sys.stderr, flush=True)
        src = ROOT / "oracle" / "programs" / name
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            # cargo's project
            proj = tmp / "cargo"
            subprocess.run(["cargo", "new", "-q", "--vcs", "none", "--name", "prog", str(proj)], check=True)
            shutil.copy(src, proj / "src" / "main.rs")
            timed(["cargo", "build", "-q"], proj)
            print(f"  {name} warming release", file=sys.stderr, flush=True)
            timed(["cargo", "build", "-q", "--release"], proj)
            cargo_exe = proj / "target" / "debug" / "prog"
            # athrad's copy
            work = tmp / "athrad"
            work.mkdir()
            shutil.copy(src, work / "main.rs")
            athrad_exe = work / "prog"
            timed([ATHRAD, "main.rs", "-o", "prog"], work)

            cargo_ms, release_ms, athrad_ms, watch_ms = [], [], [], []
            for i in range(RUNS):
                print(f"  {name} edit {i + 1}/{RUNS}", file=sys.stderr, flush=True)
                flip(proj / "src" / "main.rs", a, b)
                cargo_ms.append(timed(["cargo", "build", "-q"], proj))
                release_ms.append(timed(["cargo", "build", "-q", "--release"], proj))
                flip(work / "main.rs", a, b)
                athrad_ms.append(timed([ATHRAD, "main.rs", "-o", "prog"], work))
                if output(cargo_exe) != output(athrad_exe):
                    sys.exit(f"{name}: athrad and cargo disagree after an edit")

            # One warm watcher. It prints "built ... in N ms" per build.
            # Its own session, so stopping it also stops the driver. Killing
            # only the launcher leaves the driver running (and watching).
            w = subprocess.Popen(
                [ATHRAD, "watch", "main.rs", "-o", "prog"],
                cwd=work,
                stderr=subprocess.PIPE,
                env=env_clean(),
                start_new_session=True,
            )
            try:
                read_built(w, name)  # the first build
                for _ in range(RUNS):
                    time.sleep(0.2)
                    t = time.perf_counter()
                    flip(work / "main.rs", a, b)
                    read_built(w, name)
                    watch_ms.append((time.perf_counter() - t) * 1e3)
                    flip(proj / "src" / "main.rs", a, b)
                    timed(["cargo", "build", "-q"], proj)
                    if output(cargo_exe) != output(athrad_exe):
                        sys.exit(f"{name}: athrad watch and cargo disagree after an edit")
            finally:
                try:
                    os.killpg(w.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                w.wait()

            med = lambda xs: statistics.median(xs)
            print(
                f"{name:15} {med(cargo_ms):12.0f} {med(release_ms):8.0f} {med(athrad_ms):8.0f} {med(watch_ms):13.0f}",
                flush=True,
            )
            RESULTS["programs"].append(
                {"name": name, "cargo": cargo_ms, "release": release_ms, "athrad": athrad_ms, "watch": watch_ms}
            )
            save()


# Cargo projects: (name, files to copy in, file to edit, literal, flip).
# `big` is big.rs as a package's main.rs.
PROJECTS = [
    ("shapes: bin edit", "oracle/crates/shapes", "app/src/util.rs", "== {} ==", "== {} ==!"),
    ("shapes: lib edit", "oracle/crates/shapes", "geom/src/lib.rs", "w * h,", "w * h + 0.0,"),
    ("big (crate)", None, "src/main.rs", "let (t, s) = work3(8);", "let (t, s) = work3(9);"),
    ("popular: bin edit", "oracle/crates/popular", "src/main.rs", "with_seed(7)", "with_seed(8)"),
]


def projects():
    athrad = ATHRAD.with_name("athrad")
    print(flush=True)
    print(
        f"{'project':18} {'cargo build':>12} {'release':>8} {'athrad build':>13}   (median of {RUNS}, ms)",
        flush=True,
    )
    for label, src, file, a, b in PROJECTS:
        print(f"measuring {label}", file=sys.stderr, flush=True)
        with tempfile.TemporaryDirectory() as tmp:
            proj = Path(tmp) / "p"
            if src:
                shutil.copytree(ROOT / src, proj, ignore=shutil.ignore_patterns("target"))
            else:
                subprocess.run(["cargo", "new", "-q", "--vcs", "none", "--name", "big", str(proj)], check=True)
                shutil.copy(ROOT / "oracle" / "programs" / "big.rs", proj / "src" / "main.rs")
            timed(["cargo", "build", "-q"], proj)
            print(f"  {label} warming release", file=sys.stderr, flush=True)
            timed(["cargo", "build", "-q", "--release"], proj)
            timed([athrad, "build", "-q"], proj)
            cargo_ms, release_ms, athrad_ms = [], [], []
            for i in range(RUNS):
                print(f"  {label} edit {i + 1}/{RUNS}", file=sys.stderr, flush=True)
                flip(proj / file, a, b)
                time.sleep(0.2)
                cargo_ms.append(timed(["cargo", "build", "-q"], proj))
                time.sleep(0.2)
                release_ms.append(timed(["cargo", "build", "-q", "--release"], proj))
                time.sleep(0.2)
                athrad_ms.append(timed([athrad, "build", "-q"], proj))
            med = statistics.median
            print(
                f"{label:18} {med(cargo_ms):12.0f} {med(release_ms):8.0f} {med(athrad_ms):13.0f}",
                flush=True,
            )
            RESULTS["projects"].append(
                {"name": label, "cargo": cargo_ms, "release": release_ms, "athrad": athrad_ms}
            )
            save()


if __name__ == "__main__":
    main()
    projects()
