#!/usr/bin/env bash
#
# Profile-Guided Optimisation build for the torque NIF.
#
# Builds an instrumented NIF, runs a representative JSON workload to collect
# branch/call-frequency data, then rebuilds the NIF using that profile. The
# result is an optimised priv/native/torque_nif.so that mix loads in place of
# the plain build. Typical gain on a JSON-heavy workload is 5-15% over -O3.
#
#   ./scripts/pgo-build.sh
#
# rustc is LLVM-based, so PGO uses the same mechanism (and the same
# `llvm-profdata merge` step) as a Clang/LLVM PGO build. The one subtlety:
# the raw *.profraw counters must be merged by an llvm-profdata whose LLVM
# major version matches the one rustc was built against, otherwise the merge
# fails with a profile-format-version error. We locate a matching tool below;
# override with LLVM_PROFDATA=/path/to/llvm-profdata if detection misses.
set -euo pipefail

cd "$(dirname "$0")/.."

PGO_DIR="$PWD/target/pgo"
MERGED="$PGO_DIR/merged.profdata"
WORKLOAD="${WORKLOAD:-bench/pgo_workload.exs}"

# Must mirror native/torque_nif/.cargo/config.toml: setting RUSTFLAGS replaces
# (does not merge with) the config's rustflags, so re-state target-cpu=native
# or the SIMD build regresses.
BASE_RUSTFLAGS="-C target-cpu=native"

# rustc's LLVM major version — the profraw format is keyed to it.
RUSTC_LLVM_MAJOR="$(rustc -vV | sed -n 's/^LLVM version: \([0-9][0-9]*\).*/\1/p')"

profdata_major() { "$1" --version 2>/dev/null | sed -n 's/.*LLVM version \([0-9][0-9]*\).*/\1/p'; }

find_profdata() {
  local host sysroot cand
  host="$(rustc -vV | sed -n 's/^host: //p')"
  sysroot="$(rustc --print sysroot)"
  for cand in \
    "${LLVM_PROFDATA:-}" \
    "$sysroot/lib/rustlib/$host/bin/llvm-profdata" \
    "$(brew --prefix "llvm@$RUSTC_LLVM_MAJOR" 2>/dev/null)/bin/llvm-profdata" \
    "$(brew --prefix llvm 2>/dev/null)/bin/llvm-profdata" \
    "$(command -v llvm-profdata 2>/dev/null || true)"
  do
    [ -n "$cand" ] && [ -x "$cand" ] || continue
    if [ "$(profdata_major "$cand")" = "$RUSTC_LLVM_MAJOR" ]; then
      echo "$cand"; return 0
    fi
  done
  return 1
}

if ! PROFDATA="$(find_profdata)"; then
  echo "error: no llvm-profdata matching rustc's LLVM $RUSTC_LLVM_MAJOR found." >&2
  echo "       install a matching LLVM (rustup: 'rustup component add llvm-tools-preview';" >&2
  echo "       Homebrew: 'brew install llvm@$RUSTC_LLVM_MAJOR') or set LLVM_PROFDATA=..." >&2
  exit 1
fi
echo "==> using llvm-profdata: $PROFDATA (LLVM $RUSTC_LLVM_MAJOR)"

export TORQUE_BUILD=true

rm -rf "$PGO_DIR"
mkdir -p "$PGO_DIR"

echo "==> PGO 1/3: build instrumented NIF"
RUSTFLAGS="$BASE_RUSTFLAGS -Cprofile-generate=$PGO_DIR" mix compile --force

echo "==> PGO 2/3: collect profile data ($WORKLOAD)"
RUSTFLAGS="$BASE_RUSTFLAGS -Cprofile-generate=$PGO_DIR" mix run "$WORKLOAD"

if ! ls "$PGO_DIR"/*.profraw >/dev/null 2>&1; then
  echo "error: no *.profraw produced — did the workload exercise the NIF?" >&2
  exit 1
fi

echo "==> merging profile data into $(basename "$MERGED")"
"$PROFDATA" merge -o "$MERGED" "$PGO_DIR"/*.profraw

echo "==> PGO 3/3: rebuild using profile"
# LLVM stays quiet about functions the workload never reached unless
# -pgo-warn-missing-function is passed, so no suppression flag is needed.
RUSTFLAGS="$BASE_RUSTFLAGS -Cprofile-use=$MERGED" mix compile --force

echo "==> PGO build complete: priv/native/torque_nif.so"
