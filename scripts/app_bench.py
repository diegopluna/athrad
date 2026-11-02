#!/usr/bin/env python3
"""A real application: release builds with cargo and with Athrad's tiered
build, and how fast what each builds runs.

    python3 scripts/app_bench.py --project DIR --bin NAME --edit FILE \
        --workload bench/apps/ripgrep.txt [--rounds 3] [--runs 7]

The workload file has one command per line, `{bin}` standing for the built
program and `$VARS` expanded (blank lines and `#` comments are skipped). The tiered build's
profile comes from running the whole workload once with an instrumented
build, outside any timing. Every command must print the same thing under
both builds. Everything runs with the toolchain Athrad is pinned to.

Writes every sample to /tmp/athrad_app_bench.json.
"""

import argparse
import json
import math
import os
import shlex
import shutil
import statistics
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ATHRAD = ROOT / "target" / "release" / "athrad"
TOOLCHAIN = next(
    l.split('"')[1] for l in (ROOT / "rust-toolchain.toml").read_text().splitlines() if l.startswith("channel")
)


def env_for(project, variant):
    env = dict(os.environ)
    env.pop("RUSTFLAGS", None)
    env["RUSTUP_TOOLCHAIN"] = TOOLCHAIN
    env["CARGO_TARGET_DIR"] = str(project / "target" / variant)
    return env


def build(project, variant, hot):
    if variant == "athrad-tiered":
        cmd = [ATHRAD, "build", "--release", "-q", "--profile", hot]
    else:
        cmd = ["cargo", "build", "--release", "-q"]
    t = time.perf_counter()
    subprocess.run(cmd, cwd=project, env=env_for(project, variant), check=True)
    return time.perf_counter() - t


def commands(workload, exe):
    out = []
    for line in Path(workload).read_text().splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            out.append([os.path.expandvars(a.replace("{bin}", str(exe))) for a in shlex.split(line)])
    return out


def make_profile(project, binary, workload):
    raw = project / "target" / "athrad-profraw"
    shutil.rmtree(raw, ignore_errors=True)
    env = env_for(project, "athrad-profile")
    subprocess.run([ATHRAD, "build", "--release", "-q", "--profile-generate", raw], cwd=project, env=env, check=True)
    exe = project / "target" / "athrad-profile" / "release" / binary
    for cmd in commands(workload, exe):
        subprocess.run(cmd, capture_output=True)
    hot = project / "target" / "athrad-hot.txt"
    subprocess.run([ATHRAD, "hot", raw, "-o", hot], check=True)
    return hot


def geomean(xs):
    return math.exp(sum(math.log(x) for x in xs) / len(xs))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--project", required=True, type=Path)
    ap.add_argument("--bin", required=True)
    ap.add_argument("--edit", required=True, type=Path, help="a source file of the binary's crate")
    ap.add_argument("--workload", required=True)
    ap.add_argument("--rounds", type=int, default=3)
    ap.add_argument("--runs", type=int, default=7)
    args = ap.parse_args()
    project = args.project.resolve()
    edit_file = (project / args.edit).resolve()
    variants = ["cargo", "athrad-tiered"]

    subprocess.run(["cargo", "fetch", "-q"], cwd=project, env=env_for(project, "cargo"), check=True)
    t = time.perf_counter()
    hot = make_profile(project, args.bin, args.workload)
    profile_time = time.perf_counter() - t
    print(f"profile: {profile_time:.1f}s, {len(hot.read_text().split())} hot functions", flush=True)

    clean = {v: [] for v in variants}
    edit = {v: [] for v in variants}
    original = edit_file.read_text()
    try:
        for r in range(args.rounds):
            for v in variants:
                shutil.rmtree(project / "target" / v, ignore_errors=True)
                clean[v].append(build(project, v, hot))
                edit_file.write_text(original + f"\n// edit {r}\n")
                edit[v].append(build(project, v, hot))
                edit_file.write_text(original)
                build(project, v, hot)
                print(f"build round {r + 1}: {v} clean {clean[v][-1]:.1f}s edit {edit[v][-1]:.2f}s", flush=True)
    finally:
        edit_file.write_text(original)

    exes = {v: project / "target" / v / "release" / args.bin for v in variants}
    cmds = {v: commands(args.workload, exes[v]) for v in variants}
    n = len(cmds["cargo"])
    times = {v: [[] for _ in range(n)] for v in variants}
    mismatches = set()
    for _ in range(2):  # warm the page cache and both binaries
        for v in variants:
            for c in cmds[v]:
                subprocess.run(c, capture_output=True)
    for r in range(args.runs):
        for i in range(n):
            ref = None
            for v in variants:
                t = time.perf_counter()
                p = subprocess.run(cmds[v][i], capture_output=True)
                times[v][i].append(time.perf_counter() - t)
                got = (p.returncode, p.stdout, p.stderr.replace(str(exes[v]).encode(), b"BIN"))
                if ref is None:
                    ref = got
                elif got != ref:
                    mismatches.add(i)
        print(f"run round {r + 1}/{args.runs} done", flush=True)

    host = f"{os.uname().sysname} {os.uname().machine}, {os.cpu_count()} CPUs"
    print(f"\n{args.bin} on {host}; medians; ratios against `cargo build --release`\n")
    bc, be = statistics.median(clean["cargo"]), statistics.median(edit["cargo"])
    tc, te = statistics.median(clean["athrad-tiered"]), statistics.median(edit["athrad-tiered"])
    print(f"clean build    cargo {bc:7.2f}s   tiered {tc:7.2f}s  {tc / bc:.2f}x")
    print(f"edit build     cargo {be:7.2f}s   tiered {te:7.2f}s  {te / be:.2f}x")
    ratios = []
    for i in range(n):
        a, b = statistics.median(times["cargo"][i]), statistics.median(times["athrad-tiered"][i])
        ratios.append(b / a)
        label = " ".join(cmds["cargo"][i][1:])[:60]
        print(f"run {i:2}  cargo {a:7.3f}s   tiered {b:7.3f}s  {b / a:.2f}x  {label}")
    print(f"run geomean {geomean(ratios):.3f}x  worst {max(ratios):.2f}x")
    if mismatches:
        print("\nOUTPUT DIFFERS for commands:", sorted(mismatches))
    Path("/tmp/athrad_app_bench.json").write_text(
        json.dumps({"host": host, "clean": clean, "edit": edit, "runs": times, "mismatches": sorted(mismatches)}, indent=1)
    )
    return 1 if mismatches else 0


if __name__ == "__main__":
    sys.exit(main())
