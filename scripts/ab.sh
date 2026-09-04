#!/usr/bin/env bash
#
# A/B two revisions of the NIF under PGO, reporting instructions retired next
# to cycles.
#
#   ./scripts/ab.sh <git-rev> [workload.exs]
#
# Why not a plain `-O3` wall clock: the cdylib is built with fat LTO in one
# codegen unit, so moving any amount of source reshuffles placement everywhere.
# A deduplication pass that touched no encoder arithmetic measured +2.29%
# cycles at -O3 with instructions *down* 0.23% and frontend stalls up 30.2%,
# and +0.00% cycles once both sides were rebuilt with a profile. Instructions
# retired separate the two: work moves them, placement does not.
set -euo pipefail

cd "$(dirname "$0")/.."

REF="${1:?usage: ab.sh <git-rev> [workload.exs]}"
WORKLOAD="${2:-}"
EVENTS="instructions,cycles,stalled-cycles-frontend,L1-icache-load-misses"

command -v perf >/dev/null || { echo "ab.sh: perf is required" >&2; exit 1; }

if [ -z "$WORKLOAD" ]; then
  WORKLOAD="$(mktemp -t torque-ab-XXXXXX.exs)"
  trap 'rm -f "$WORKLOAD"' EXIT
  cat >"$WORKLOAD" <<'EOF'
recs = for i <- 1..2000, do: %{"id" => i, "name" => "user_#{i}", "score" => i * 1.5}
json = Jason.encode!(recs)
Torque.encode(recs)
for _ <- 1..1500 do
  Torque.encode(recs)
  Torque.decode(json)
end
EOF
fi

BASE="$(git rev-parse --abbrev-ref HEAD)"
[ "$BASE" = "HEAD" ] && BASE="$(git rev-parse HEAD)"
STASHED=""
if ! git diff --quiet || ! git diff --cached --quiet; then
  git stash push -q -m "ab.sh $(date +%s)"
  STASHED=1
fi
restore() {
  git checkout -q "$BASE"
  [ -n "$STASHED" ] && git stash pop -q || true
}
trap 'restore' EXIT

measure() {
  TORQUE_BUILD=true ./scripts/pgo-build.sh >/dev/null 2>&1
  # Compile the bench environment outside the measured command: `mix run`
  # builds it on first use, and perf would count that as the workload.
  TORQUE_BUILD=true MIX_ENV=bench mix run "$WORKLOAD" >/dev/null 2>&1
  TORQUE_BUILD=true MIX_ENV=bench perf stat -e "$EVENTS" -x, -r 3 -- \
    mix run "$WORKLOAD" 2>&1 | awk -F, '$1 ~ /^[0-9]+$/ {print $3"="$1}'
}

echo "==> building $REF"
git checkout -q "$REF"
BEFORE="$(measure)"

echo "==> building $BASE"
git checkout -q "$BASE"
AFTER="$(measure)"

echo
printf '%-26s %16s %16s %9s\n' counter "$REF" "$BASE" delta
for key in $(echo "$BEFORE" | cut -d= -f1); do
  b="$(echo "$BEFORE" | grep "^$key=" | cut -d= -f2)"
  a="$(echo "$AFTER" | grep "^$key=" | cut -d= -f2)"
  [ -n "$b" ] && [ -n "$a" ] && [ "$b" -ne 0 ] || continue
  printf '%-26s %16s %16s %+8.2f%%\n' "$key" "$b" "$a" \
    "$(awk -v b="$b" -v a="$a" 'BEGIN{printf "%.2f", (a-b)/b*100}')"
done
echo
echo "Instructions moved => real work. Only cycles moved => placement; PGO absorbs it."
