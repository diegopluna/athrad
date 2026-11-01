#!/usr/bin/env bash
# Milestone-0 baseline: `cargo build` of the add-and-print program, clean and
# after a one-line edit. Prints wall times in seconds; `--timings` reports
# land in $DIR/target/cargo-timings.
set -euo pipefail
RUNS=${RUNS:-10}
DIR=${DIR:-$(mktemp -d)/add}
ROOT=$(cd "$(dirname "$0")/.." && pwd)

cargo new -q --vcs none --name add "$DIR"
cp "$ROOT/oracle/programs/add.rs" "$DIR/src/main.rs"
cd "$DIR"

now() { python3 -c 'import time; print(time.perf_counter())'; }
time_it() { local t0 t1; t0=$(now); "$@" >/dev/null 2>&1; t1=$(now); python3 -c "print(f'{$t1-$t0:.3f}')"; }

echo "rustc: $(rustc -V)"
echo "host:  $(uname -sm)"
echo "clean cargo build (cargo clean before each):"
for _ in $(seq "$RUNS"); do cargo clean -q; time_it cargo build --timings; done | sort -n | tr '\n' ' '; echo
echo "one-line edit, cargo build (add(2, 3) <-> add(2, 4)):"
cargo build -q
for i in $(seq "$RUNS"); do
  if (( i % 2 )); then sed -i.bak 's/add(2, 3)/add(2, 4)/' src/main.rs; else sed -i.bak 's/add(2, 4)/add(2, 3)/' src/main.rs; fi
  time_it cargo build --timings
done | sort -n | tr '\n' ' '; echo
echo "no-op cargo build:"
for _ in $(seq "$RUNS"); do time_it cargo build; done | sort -n | tr '\n' ' '; echo
echo "timings: $DIR/target/cargo-timings"
