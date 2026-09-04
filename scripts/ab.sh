#!/usr/bin/env bash
# A/B two NIF revisions per operation under PGO.
#
#   ./scripts/ab.sh <git-rev> [op ...]
#
#   AB_TARGET_MS=600     measured work per operation
#   AB_REPEATS=4         perf repetitions
#   AB_KEEP=1            keep temporary worktrees
#   AB_WORKLOAD=x.exs    raw workload without baseline subtraction
#
# Both revisions use the invoking checkout's workload and run in disposable
# worktrees. PGO limits layout noise; matched baselines remove VM startup and
# fixture construction; perf variance supplies each result's resolution.
set -euo pipefail

cd "$(dirname "$0")/.."
REPO="$PWD"

REF="${1:?usage: ab.sh <git-rev> [op ...]}"
shift || true
SELECTED=("$@")

# Increase AB_TARGET_MS when the default ~2% resolution is insufficient.
TARGET_MS="${AB_TARGET_MS:-600}"
REPEATS="${AB_REPEATS:-4}"
# Named AB_WORKLOAD, not WORKLOAD: pgo-build.sh reads WORKLOAD for its own
# training workload, and one exported variable meaning both is a trap.
WORKLOAD="${AB_WORKLOAD:-}"
EVENTS="instructions,cycles"

if [ "$REF" = "--report" ]; then
  REPORT_ONLY="${SELECTED[0]:?usage: ab.sh --report <ab-last.tsv>}"
  REF_SHA=""; BASE=""; SCRATCH="$(mktemp -d -t torque-ab-XXXXXX)"
else
  REPORT_ONLY=""
  command -v perf >/dev/null || { echo "ab.sh: perf is required" >&2; exit 1; }

  # Resolve to full SHAs up front: a ref can move between runs, and a report
  # that names one cannot be reproduced from its own output.
  REF_SHA="$(git rev-parse --verify "$REF^{commit}")" ||
    { echo "ab.sh: cannot resolve $REF" >&2; exit 1; }
  BASE="$(git rev-parse HEAD)"
  SCRATCH="$(mktemp -d -t torque-ab-XXXXXX)"
fi

# One trap for everything: a second `trap ... EXIT` replaces the first rather
# than adding to it, which is how the generated workload used to leak.
cleanup() {
  local status=$?
  if [ -n "${AB_KEEP:-}" ]; then
    echo "ab.sh: worktrees kept at $SCRATCH" >&2
    return $status
  fi
  for dir in "$SCRATCH"/ref "$SCRATCH"/base; do
    [ -d "$dir" ] && git -C "$REPO" worktree remove --force "$dir" >/dev/null 2>&1
  done
  rm -rf "$SCRATCH"
  return $status
}
trap cleanup EXIT

# Use one workload for both revisions. Disable BEAM busy-wait so idle scheduler
# loops do not contaminate instruction and cycle counts.
BEAM_OPTS="+sbwt none +sbwtdcpu none +sbwtdio none"

run_in() {
  ( cd "$1"; shift
    export TORQUE_BUILD=true MIX_ENV=bench TORQUE_SOURCE_ROOT="$REPO"
    export ELIXIR_ERL_OPTIONS="$BEAM_OPTS"
    "$@" )
}

# Builds one revision in its own worktree and leaves it ready to measure.
prepare() {
  local rev="$1" dir="$2"
  git -C "$REPO" worktree add -q --detach "$dir" "$rev"
  if [ -d "$REPO/deps" ]; then cp -R "$REPO/deps" "$dir/deps"; fi
  # A worktree gets tracked files only, and the toolchain pin is untracked, so
  # without this `mix` does not resolve inside it.
  for cfg in .tool-versions .envrc; do
    if [ -f "$REPO/$cfg" ]; then cp "$REPO/$cfg" "$dir/$cfg"; fi
  done
  # Profile both revisions with the invoking checkout's workload.
  mkdir -p "$dir/bench"
  cp "$REPO/bench/fixtures.exs" "$REPO/bench/ops.exs" "$REPO/bench/pgo_workload.exs" \
    "$dir/bench/"
  if [ -n "$WORKLOAD" ]; then cp "$WORKLOAD" "$dir/ab-custom.exs"; fi

  local log="$dir/ab-build.log"
  (
    cd "$dir"
    export TORQUE_BUILD=true
    export MIX_ENV=bench
    export TORQUE_SOURCE_ROOT="$REPO"
    mix deps.get
    # Profile in the measured MIX_ENV; priv/native is shared across environments
    # and would otherwise be overwritten by a plain build.
    ./scripts/pgo-build.sh
  ) >"$log" 2>&1 || { echo "ab.sh: build failed for $rev" >&2; tail -20 "$log" >&2; exit 1; }
  sha256sum "$dir/priv/native/torque_nif.so" | cut -d' ' -f1 >"$dir/ab-pgo.sha"
}

# Prints instructions, cycles, and their relative standard deviations. A
# command unsupported by one revision is reported as FAILED without aborting
# the remaining operations.
measure() {
  local dir="$1"; shift
  # Warm generated BEAM artifacts before measuring.
  if ! run_in "$dir" mix run "$@" >"$dir/ab-last.log" 2>&1; then
    { echo "ab.sh: [$(basename "$dir")] $* failed:"; tail -5 "$dir/ab-last.log"; } >&2
    echo "-1 0 -1 0"
    return 0
  fi

  local out
  out="$(run_in "$dir" perf stat -e "$EVENTS" -x, -r "$REPEATS" -- mix run "$@" 2>&1 |
    awk -F, '$1 ~ /^[0-9]+$/ { gsub(/%/, "", $4); print $3, $1, ($4 == "" ? 0 : $4) }')"

  # A hash mismatch cannot `exit` here because `measure` runs in a subshell.
  local now
  now="$(sha256sum "$dir/priv/native/torque_nif.so" | cut -d' ' -f1)"
  if [ "$now" != "$(cat "$dir/ab-pgo.sha")" ]; then
    echo "ab.sh: NIF in $dir rebuilt after profiling; measurement is not PGO" >&2
    touch "$SCRATCH/failed"
  fi

  awk -v out="$out" 'BEGIN {
    n = split(out, lines, "\n")
    for (i = 1; i <= n; i++) { split(lines[i], f, " "); v[f[1]] = f[2]; s[f[1]] = f[3] }
    printf "%d %s %d %s\n", v["instructions"], s["instructions"], v["cycles"], s["cycles"]
  }'
}

# Renders a saved measurement file. Takes the revision SHAs from the file, so a
# report can be reproduced from its own data.
render() {
  awk -F'\t' '
function abs(x) { return x < 0 ? -x : x }

$1 == "REVS" { ref = substr($2, 1, 10); head = substr($3, 1, 10); next }
$1 == "META" { ops[++n] = $2; basekey[$2] = $3; control[$2] = $4; reps[$2] = $5; next }
{ v[$1 "/" $2 "/" $3] = $4; sd[$1 "/" $2 "/" $3] = $5; cv[$1 "/" $2 "/" $3] = $6; csd[$1 "/" $2 "/" $3] = $7 }

END {
  printf "%-22s %14s %14s %9s   %s\n", "operation", ref, head, "delta", "verdict"
  printf "%-22s %14s %14s %9s   %s\n", "----------------------", "--------------", "--------------", "---------", "-------"

  for (i = 1; i <= n; i++) {
    op = ops[i]; bk = basekey[op]

    # Work = measured run minus the baseline that built the same fixtures.
    # perf reports a relative standard deviation; the subtraction combines the
    # two in absolute terms.
    for (m = 0; m < 2; m++) {
      side = m ? "base" : "ref"
      wi[side] = v[side "/op/" op] - v[side "/base/" bk]
      si[side] = sqrt((v[side "/op/" op] * sd[side "/op/" op] / 100) ^ 2 \
                    + (v[side "/base/" bk] * sd[side "/base/" bk] / 100) ^ 2)
      wc[side] = cv[side "/op/" op] - cv[side "/base/" bk]
      sc[side] = sqrt((cv[side "/op/" op] * csd[side "/op/" op] / 100) ^ 2 \
                    + (cv[side "/base/" bk] * csd[side "/base/" bk] / 100) ^ 2)
    }

    if (v["ref/op/" op] < 0 || v["base/op/" op] < 0 ||
        v["ref/base/" bk] < 0 || v["base/base/" bk] < 0) {
      side = v["ref/op/" op] < 0 || v["ref/base/" bk] < 0 ? ref : head
      printf "%-22s %14s %14s %9s   %s\n", op, "-", "-", "-", \
        sprintf("FAILED   does not run at %s", side)
      failed++; continue
    }

    if (wi["ref"] <= 0 || wi["base"] <= 0) {
      printf "%-22s %14s %14s %9s   %s\n", op, "-", "-", "-", \
        "INVALID  baseline exceeds the measured run; raise AB_TARGET_MS"
      bad++; continue
    }

    di = wi["base"] / wi["ref"] - 1
    dc = wc["base"] / wc["ref"] - 1
    # Relative uncertainty of the ratio, from both sides.
    ni = sqrt((si["ref"] / wi["ref"]) ^ 2 + (si["base"] / wi["base"]) ^ 2)
    nc = sqrt((sc["ref"] / wc["ref"]) ^ 2 + (sc["base"] / wc["base"]) ^ 2)

    # Every verdict is reported against the resolution that produced it: three
    # sigma on the subtracted work, which is what "no change" is worth here.
    res = 3 * ni
    res_c = 3 * nc

    moved_i = (abs(di) > res && abs(di) > 0.001)

    # Require resolved cycle movement for LAYOUT. WEAK means instruction
    # movement is unresolved, not merely that its resolution is coarse.
    moved_c = (abs(dc) > res_c && abs(dc) > 0.005 && res_c <= 0.10)

    weak = (res > 0.06 && !moved_i)

    if (moved_i)      verdict = sprintf("%s  %+.2f%% work (>%.1f%%)", di < 0 ? "BETTER " : "WORSE  ", di * 100, res * 100)
    else if (weak)    verdict = sprintf("WEAK     cannot resolve better than %.1f%%", res * 100)
    else if (moved_c) verdict = sprintf("LAYOUT   %+.2f%% cycles, instructions flat within %.1f%%", dc * 100, res * 100)
    else              verdict = sprintf("=        within %.1f%%", res * 100)

    if (control[op] == 1 && (moved_i || moved_c)) {
      verdict = verdict "  <- CONTROL"
      controls_moved++
    }

    if (moved_i) { if (di < 0) better++; else worse++ }
    else if (weak) weakn++
    else if (moved_c) layout++

    printf "%-22s %14d %14d %+8.2f%%   %s\n", op, wi["ref"], wi["base"], di * 100, verdict
  }

  printf "\n%d better, %d worse, %d layout-only, %d unchanged", \
    better + 0, worse + 0, layout + 0, \
    n - (better + worse + layout + weakn + bad + failed) + 0
  if (weakn) printf ", %d too weak (raise AB_TARGET_MS)", weakn
  if (bad) printf ", %d invalid", bad
  if (failed) printf ", %d failed to run", failed
  printf "\n"
  if (controls_moved) {
    printf "\n!! %d CONTROL operation(s) moved. A control exercises the code around a\n", controls_moved
    printf "   branch without entering it, so either the change reached further than the\n"
    printf "   branch it names — expected when comparing across a long span of history —\n"
    printf "   or the measurement drifted. For a change that claims to be local, it is\n"
    printf "   the second.\n"
  }
  printf "\nColumns are instructions retired for the operation alone: BEAM startup, fixture\n"
  printf "construction and setup are measured separately and subtracted. Instructions moved\n"
  printf "=> real work. Only cycles moved => placement, which PGO mostly absorbs. The\n"
  printf "percentage after each verdict is the 3-sigma resolution for that row: raise\n"
  printf "AB_TARGET_MS to narrow it, or name fewer operations on the command line.\n"
}' "$1"
}

# Re-render a saved measurement without building or measuring anything.
if [ -n "$REPORT_ONLY" ]; then
  render "$REPORT_ONLY"
  exit 0
fi

echo "==> $REF is $REF_SHA"
echo "==> HEAD  is $BASE"
prepare "$REF_SHA" "$SCRATCH/ref"
prepare "$BASE" "$SCRATCH/base"

RECORDS="$SCRATCH/records"
: >"$RECORDS"
declare -A BASEOF=()
declare -A CONTROL=()
declare -A REPS=()

# Raw workload mode: no baseline subtraction.
if [ -n "$WORKLOAD" ]; then
  for side in ref base; do
    read -r i isd c csd <<<"$(measure "$SCRATCH/$side" ab-custom.exs)"
    printf '%s\top\tcustom\t%s\t%s\t%s\t%s\n' "$side" "$i" "$isd" "$c" "$csd" >>"$RECORDS"
    printf '%s\tbase\tcustom\t0\t0\t0\t0\n' "$side" >>"$RECORDS"
  done
  OPS=(custom)
  BASEOF[custom]=custom
else
  # Named-operation mode.
  ALL_OPS=()
  # Warm the registry before parsing its output.
  run_in "$SCRATCH/base" mix run bench/ops.exs list >/dev/null 2>&1
  while IFS=$'\t' read -r op key; do
    [ -n "$op" ] && [ -n "$key" ] || continue
    ALL_OPS+=("$op")
    BASEOF["$op"]="$key"
  done < <(run_in "$SCRATCH/base" mix run bench/ops.exs plan 2>/dev/null)

  [ ${#ALL_OPS[@]} -gt 0 ] || { echo "ab.sh: bench/ops.exs listed no operations" >&2; exit 1; }

  # Operations named CONTROL are expected *not* to move. One that does is
  # evidence about the measurement, not about the change.
  while IFS= read -r line; do
    case "$line" in
      *"CONTROL"*) CONTROL["${line%% *}"]=1 ;;
    esac
  done < <(run_in "$SCRATCH/base" mix run bench/ops.exs describe 2>/dev/null)

  if [ ${#SELECTED[@]} -gt 0 ]; then
    OPS=()
    for want in "${SELECTED[@]}"; do
      [ -n "${BASEOF[$want]:-}" ] ||
        { echo "ab.sh: no such op: $want (try: mix run bench/ops.exs list)" >&2; exit 1; }
      OPS+=("$want")
    done
  else
    OPS=("${ALL_OPS[@]}")
  fi

  # Calibrate once so both revisions execute equal work.
  echo "==> calibrating ${#OPS[@]} operations to ~${TARGET_MS} ms each"
  for op in "${OPS[@]}"; do
    REPS["$op"]="$(run_in "$SCRATCH/base" mix run bench/ops.exs calibrate "$op" "$TARGET_MS" 2>/dev/null | tail -1)"
    [ -n "${REPS[$op]}" ] || { echo "ab.sh: calibration failed for $op" >&2; exit 1; }
  done

  # One baseline per distinct baseline group, per side.
  declare -A BASE_REP=()
  for op in "${OPS[@]}"; do BASE_REP["${BASEOF[$op]}"]="$op"; done

  total=$(( (${#OPS[@]} + ${#BASE_REP[@]}) * 2 ))
  echo "==> measuring $total runs at perf -r $REPEATS"

  done_runs=0
  for side in ref base; do
    for key in "${!BASE_REP[@]}"; do
      read -r i isd c csd <<<"$(measure "$SCRATCH/$side" bench/ops.exs baseline "${BASE_REP[$key]}")"
      printf '%s\tbase\t%s\t%s\t%s\t%s\t%s\n' "$side" "$key" "$i" "$isd" "$c" "$csd" >>"$RECORDS"
      done_runs=$((done_runs + 1)); printf '\r    %d/%d' "$done_runs" "$total"
    done
    for op in "${OPS[@]}"; do
      read -r i isd c csd <<<"$(measure "$SCRATCH/$side" bench/ops.exs run "$op" "${REPS[$op]}")"
      printf '%s\top\t%s\t%s\t%s\t%s\t%s\n' "$side" "$op" "$i" "$isd" "$c" "$csd" >>"$RECORDS"
      done_runs=$((done_runs + 1)); printf '\r    %d/%d' "$done_runs" "$total"
    done
  done
  printf '\r%*s\r' 20 ''
  if [ -f "$SCRATCH/failed" ]; then exit 1; fi
fi

# Save measurements so report wording can be rerendered without rebuilding.

{
  printf 'REVS\t%s\t%s\n' "$REF_SHA" "$BASE"
  for op in "${OPS[@]}"; do
    printf 'META\t%s\t%s\t%s\t%s\n' "$op" "${BASEOF[$op]}" "${CONTROL[$op]:-0}" "${REPS[$op]:-1}"
  done
  cat "$RECORDS"
} >"$REPO/ab-last.tsv"

echo "==> measurements saved to ab-last.tsv (re-render with: ./scripts/ab.sh --report ab-last.tsv)"
render "$REPO/ab-last.tsv"
