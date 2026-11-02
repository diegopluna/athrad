#!/usr/bin/env python3
"""Where a release build's time goes, with cargo and with Athrad's tiered
build: every pass and LLVM thread of one crate's compile, and every crate of
a clean build.

    python3 scripts/compile_profile.py --project ~/rg --crate rg \
        --edit crates/core/main.rs --hot target/athrad-hot.txt --out /tmp/prof

For each of `cargo` and `athrad` it writes into OUT:

* `{v}-clean.json`: the crates of a clean `--release` build (`--timings`).
* `{v}-edit.trace.json`: Chrome trace of the crate's compile after a one-line
  edit, from `rustc -Zself-profile` (passes, queries, LLVM workers, and
  Athrad's own split, lowering and Cranelift).
* `{v}-edit.summary.json`: self time per query and activity (`summarize`).
* `{v}-edit.wall.json`: wall time of that compile, three runs.

Needs measureme's `crox` and `summarize` on PATH.
"""

import argparse
import json
import os
import re
import shlex
import shutil
import subprocess
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ATHRAD = ROOT / "target" / "release" / "athrad"
TOOLCHAIN = next(
    l.split('"')[1] for l in (ROOT / "rust-toolchain.toml").read_text().splitlines() if l.startswith("channel")
)


def env_for(project, v):
    env = dict(os.environ)
    env.pop("RUSTFLAGS", None)
    env["RUSTUP_TOOLCHAIN"] = TOOLCHAIN
    env["CARGO_TARGET_DIR"] = str(project / "target" / f"prof-{v}")
    return env


def cargo_cmd(v, hot, extra):
    if v == "athrad":
        return [str(ATHRAD), "build", "--release", "--profile", str(hot), *extra]
    return ["cargo", "build", "--release", *extra]


def unit_data(html):
    m = re.search(r"const UNIT_DATA = (\[.*?\]);\n", html, re.S)
    return json.loads(m.group(1)) if m else []


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--project", required=True, type=Path)
    ap.add_argument("--crate", required=True)
    ap.add_argument("--edit", required=True, type=Path)
    ap.add_argument("--hot", required=True, type=Path)
    ap.add_argument("--out", required=True, type=Path)
    args = ap.parse_args()
    project = args.project.resolve()
    hot = (project / args.hot).resolve()
    out = args.out
    out.mkdir(parents=True, exist_ok=True)
    edit = project / args.edit
    original = edit.read_text()

    for v in ["cargo", "athrad"]:
        env = env_for(project, v)
        shutil.rmtree(project / "target" / f"prof-{v}", ignore_errors=True)
        # The clean build, crate by crate.
        t = time.perf_counter()
        subprocess.run(cargo_cmd(v, hot, ["-q", "--timings"]), cwd=project, env=env, check=True)
        clean_wall = time.perf_counter() - t
        timings = sorted((project / "target" / f"prof-{v}" / "cargo-timings").glob("cargo-timing-*.html"))
        units = unit_data(timings[-1].read_text())
        (out / f"{v}-clean.json").write_text(json.dumps({"wall": clean_wall, "units": units}))

        # The crate's compile after an edit, as cargo runs it.
        try:
            edit.write_text(original + "\n// profile\n")
            p = subprocess.run(cargo_cmd(v, hot, ["-vv"]), cwd=project, env=env, capture_output=True, text=True)
        finally:
            edit.write_text(original)
        # The command, with the environment cargo sets; a package description
        # can span lines, so the command can too.
        cmd = next(
            c for c in re.findall(r"Running `(.*?)`\n", p.stderr, re.S) if f"--crate-name {args.crate} " in c
        )
        prof_dir = out / f"{v}-selfprofile"
        shutil.rmtree(prof_dir, ignore_errors=True)
        extra = f" -Zself-profile={prof_dir} -Zself-profile-events=default,args"
        run_env = dict(env, RUSTC_BOOTSTRAP="1", ATHRAD_RUSTC_ARGS="1", ATHRAD_HOT=str(hot))
        walls = []
        for i in range(3):
            edit.write_text(original + f"\n// profile {i}\n")
            t = time.perf_counter()
            subprocess.run(["bash", "-c", cmd + (extra if i == 2 else "")], cwd=project, env=run_env, check=True,
                           capture_output=True)
            walls.append(time.perf_counter() - t)
        edit.write_text(original)
        (out / f"{v}-edit.wall.json").write_text(json.dumps(walls))
        data = sorted(prof_dir.glob("*.mm_profdata"))[-1]
        stem = str(data)[: -len(".mm_profdata")]
        subprocess.run(["crox", "--minimum-duration", "100", stem], cwd=out, check=True, capture_output=True)
        shutil.move(out / "chrome_profiler.json", out / f"{v}-edit.trace.json")
        s = subprocess.run(["summarize", "summarize", "--json", stem], cwd=out, capture_output=True, text=True, check=True)
        (out / f"{v}-edit.summary.json").write_text(s.stdout)
        print(f"{v}: clean {clean_wall:.1f}s, edit {sorted(walls)[1]:.2f}s", flush=True)
    edit.write_text(original)


if __name__ == "__main__":
    main()
