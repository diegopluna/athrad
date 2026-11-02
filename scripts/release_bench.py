#!/usr/bin/env python3
"""Release builds: how long they take, and how fast what they build runs.

Builds the workloads in bench/release under `cargo build --release` and under
each candidate setting, then runs every binary, interleaving the settings so
machine drift lands on all of them alike. A setting passes only if every
binary prints exactly what cargo's does.

    python3 scripts/release_bench.py [--rounds 3] [--runs 7] [--variants a,b]

Writes every sample to /tmp/athrad_release_bench.json.
"""

import argparse
import json
import math
import os
import shutil
import statistics
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BENCH = ROOT / "bench" / "release"
WORKLOADS = ["regex", "json", "hashmap", "sort", "deflate", "sha", "numeric", "nbody", "btree", "strings"]
EDIT = BENCH / "json" / "src" / "main.rs"

# Each variant is extra environment for cargo. `cargo` is the reference.
VARIANTS = {
    "cargo": {},
    "lto-off": {"CARGO_PROFILE_RELEASE_LTO": "off"},
    "threads": {"RUSTFLAGS": f"-Zthreads={os.cpu_count()}"},
    "lto-off+threads": {"CARGO_PROFILE_RELEASE_LTO": "off", "RUSTFLAGS": f"-Zthreads={os.cpu_count()}"},
    "opt2": {"CARGO_PROFILE_RELEASE_OPT_LEVEL": "2"},
    "share-generics": {"RUSTFLAGS": "-Zshare-generics=yes"},
    "cgu256": {"CARGO_PROFILE_RELEASE_CODEGEN_UNITS": "256"},
    # `athrad build --release --profile`: LLVM for what the profile found
    # hot, Cranelift for the rest. Needs target/release/athrad.
    "athrad-tiered": {},
}

ATHRAD = ROOT / "target" / "release" / "athrad"
PROFILE = BENCH / "target" / "athrad-hot.txt"


def make_profile():
    """Collect the profile `athrad-tiered` builds with, outside any timing:
    an instrumented build, every workload run once, the counts merged."""
    raw = BENCH / "target" / "athrad-profraw"
    shutil.rmtree(raw, ignore_errors=True)
    env = env_for("cargo")
    env["CARGO_TARGET_DIR"] = str(BENCH / "target" / "athrad-profile")
    subprocess.run([ATHRAD, "build", "--release", "-q", "--profile-generate", raw], cwd=BENCH, env=env, check=True)
    for w in WORKLOADS:
        subprocess.run([BENCH / "target" / "athrad-profile" / "release" / w], check=True, capture_output=True)
    subprocess.run([ATHRAD, "hot", raw, "-o", PROFILE], check=True)


def env_for(name):
    env = dict(os.environ)
    env.pop("RUSTFLAGS", None)
    env["RUSTC_BOOTSTRAP"] = "1"  # the same for every variant: it changes no code
    env["CARGO_TARGET_DIR"] = str(BENCH / "target" / name)
    env.update(VARIANTS[name])
    return env


def cargo_build(name):
    if name == "athrad-tiered":
        cmd = [ATHRAD, "build", "--release", "-q", "--profile", PROFILE]
    else:
        cmd = ["cargo", "build", "--release", "-q"]
    t = time.perf_counter()
    subprocess.run(cmd, cwd=BENCH, env=env_for(name), check=True)
    return time.perf_counter() - t


def run(name, workload):
    exe = BENCH / "target" / name / "release" / workload
    t = time.perf_counter()
    out = subprocess.run([exe], check=True, capture_output=True).stdout
    return time.perf_counter() - t, out


def geomean(xs):
    return math.exp(sum(math.log(x) for x in xs) / len(xs))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rounds", type=int, default=3, help="clean builds per variant")
    ap.add_argument("--runs", type=int, default=7, help="runs per binary per variant")
    ap.add_argument("--variants", default=",".join(VARIANTS))
    args = ap.parse_args()
    names = args.variants.split(",")
    assert names[0] == "cargo", "the first variant is the reference"

    # Fetch once, outside any timing.
    subprocess.run(["cargo", "fetch", "-q"], cwd=BENCH, check=True)
    if "athrad-tiered" in names:
        make_profile()
    clean = {n: [] for n in names}
    edit = {n: [] for n in names}
    original = EDIT.read_text()
    try:
        for r in range(args.rounds):
            for n in names:
                shutil.rmtree(BENCH / "target" / n, ignore_errors=True)
                clean[n].append(cargo_build(n))
                EDIT.write_text(original + f"// edit {r}\n")
                edit[n].append(cargo_build(n))
                EDIT.write_text(original)
                cargo_build(n)
                print(f"build round {r + 1}: {n} clean {clean[n][-1]:.2f}s edit {edit[n][-1]:.2f}s", flush=True)
    finally:
        EDIT.write_text(original)

    times = {n: {w: [] for w in WORKLOADS} for n in names}
    reference = {}
    mismatches = []
    for r in range(args.runs):
        for w in WORKLOADS:
            for n in names:
                t, out = run(n, w)
                times[n][w].append(t)
                if n == "cargo":
                    reference.setdefault(w, out)
                elif out != reference.get(w, out):
                    mismatches.append((n, w))
        print(f"run round {r + 1}/{args.runs} done", flush=True)

    host = f"{os.uname().sysname} {os.uname().machine}, {os.cpu_count()} CPUs"
    print(f"\n{host}; medians; ratios against `cargo build --release`\n")
    print(f"{'':16}" + "".join(f"{n:>16}" for n in names))
    base_clean = statistics.median(clean["cargo"])
    base_edit = statistics.median(edit["cargo"])
    print(f"{'clean build':16}" + "".join(f"{statistics.median(clean[n]):>9.2f}s {statistics.median(clean[n]) / base_clean:4.2f}x" for n in names))
    print(f"{'edit json':16}" + "".join(f"{statistics.median(edit[n]):>9.2f}s {statistics.median(edit[n]) / base_edit:4.2f}x" for n in names))
    ratios = {n: [] for n in names}
    for w in WORKLOADS:
        base = statistics.median(times["cargo"][w])
        row = f"{'run ' + w:16}"
        for n in names:
            m = statistics.median(times[n][w])
            ratios[n].append(m / base)
            row += f"{m:>9.3f}s {m / base:4.2f}x"
        print(row)
    print(f"{'run geomean':16}" + "".join(f"{'':>11}{geomean(ratios[n]):4.2f}x" for n in names))
    print(f"{'run worst':16}" + "".join(f"{'':>11}{max(ratios[n]):4.2f}x" for n in names))
    if mismatches:
        print("\nOUTPUT DIFFERS from cargo's:", sorted(set(mismatches)))

    Path("/tmp/athrad_release_bench.json").write_text(
        json.dumps({"host": host, "clean": clean, "edit": edit, "runs": times, "mismatches": mismatches}, indent=1)
    )
    return 1 if mismatches else 0


if __name__ == "__main__":
    sys.exit(main())
