#!/usr/bin/env bash
# Acceptance gate: run the integration suites that actually exercise the app.
#
# Every integration test in this workspace is `#[ignore]`d, because each one opens
# real iroh endpoints and must run serially. That means `cargo test --workspace`
# runs ZERO of them — it compiles the world and executes only unit tests. Nothing
# about a green `cargo test` says the app syncs files. This script is the thing
# that does.
#
# Run it before cutting any release. See docs/testing.md.
#
#   bash scripts/test-acceptance.sh              # every suite
#   bash scripts/test-acceptance.sh live_folder  # one or more suites by name
#
# Exit code is non-zero if any suite fails.
set -uo pipefail

# Ordered cheapest-first, so a broken build or a broken basic sync fails fast
# rather than 40 minutes in. "crate:test"
ALL_SUITES=(
  "seed-core:docs_spike"
  "seed-core:persistence"
  "seed-core:keystore"
  "seed-core:loopback"
  "seed-core:live_folder"
  "seed-core:health"
  "seed-core:presence"
  "seed-core:member_names"
  "seed-core:discovery"
  "seed-core:rendezvous"
  "seed-core:isolation"
  "seed-core:resume"
  "seed-core:gc"
  "seed-core:tombstone_race"
  "seed-core:health_quiesce"
  "seed-core:multi_master"
  "seed-daemon:loopback_ipc"
  "seed-daemon:health_ipc"
)

suites=()
if [ "$#" -gt 0 ]; then
  for want in "$@"; do
    for s in "${ALL_SUITES[@]}"; do
      [ "${s#*:}" = "$want" ] && suites+=("$s")
    done
  done
  if [ "${#suites[@]}" -eq 0 ]; then
    echo "no matching suites; known suites:"
    printf '  %s\n' "${ALL_SUITES[@]#*:}"
    exit 2
  fi
else
  suites=("${ALL_SUITES[@]}")
fi

# Build once up front so per-suite timings reflect test time, not compile time.
echo "==> building test binaries"
cargo test --workspace --no-run || { echo "build failed"; exit 1; }

names=()
oks=()
mins=()
all_started=$SECONDS

for s in "${suites[@]}"; do
  crate="${s%%:*}"
  test="${s#*:}"
  echo
  echo "==> $crate :: $test"
  started=$SECONDS
  # --test-threads 1: these open real endpoints and bind real sockets; running
  # them concurrently makes failures meaningless.
  if cargo test -p "$crate" --test "$test" -- --ignored --nocapture --test-threads 1; then
    oks+=("PASS")
  else
    oks+=("FAIL")
    echo "FAILED: $test"
  fi
  names+=("$crate/$test")
  mins+=("$(( (SECONDS - started) / 60 ))")
done

echo
echo "==> summary ($(( (SECONDS - all_started) / 60 )) min total)"
failed=0
for i in "${!names[@]}"; do
  printf '  %-4s %-32s %5s min\n' "${oks[$i]}" "${names[$i]}" "${mins[$i]}"
  [ "${oks[$i]}" = "FAIL" ] && failed=$((failed + 1))
done

if [ "$failed" -gt 0 ]; then
  echo
  echo "$failed suite(s) failed — do not release."
  exit 1
fi
echo
echo "all suites passed"
