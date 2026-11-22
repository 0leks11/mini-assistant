#!/usr/bin/env bash
# Host-only quality checks. Never source .env or launch the assistant here.
# Each required stage runs even if an earlier stage fails.
set -uo pipefail

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)" || exit 2
cd -- "$SCRIPT_DIR/.." || exit 2

CHECK_RUN_LOG_DIR="$(mktemp -d "${TMPDIR:-/tmp}/gpt-mini-check.XXXXXX")" || {
  printf 'Cannot create a private directory for check logs.\n' >&2
  exit 2
}
stage_names=()
stage_codes=()
failures=0

run_stage() {
  local name="$1"
  shift
  local code=0
  local log="$CHECK_RUN_LOG_DIR/$name.log"
  printf '\nRunning %s...\n' "$name"
  if "$@" >"$log" 2>&1; then
    printf 'PASS %s\n' "$name"
  else
    code=$?
    failures=$((failures + 1))
    printf 'FAIL %s (exit=%s)\n' "$name" "$code"
    printf '%s\n' 'Failure excerpt (last 80 lines, at most 16384 bytes):'
    LC_ALL=C tail -n 80 -- "$log" | LC_ALL=C tail -c 16384
    printf '\nFull stage log: %s\n' "$log"
  fi
  stage_names+=("$name")
  stage_codes+=("$code")
}

printf 'Full check logs: %s\n' "$CHECK_RUN_LOG_DIR"
run_stage format cargo fmt --all -- --check
run_stage lint cargo clippy --locked --all-targets -- -D warnings
run_stage rust cargo test --locked --all-targets
run_stage vendor cargo test --locked --manifest-path vendor/rustpotter-ref/Cargo.toml --no-default-features
run_stage python python3 -B -m unittest discover -s tests -p 'test_*.py' -v

printf '\nCheck summary\n'
for ((i = 0; i < ${#stage_names[@]}; i++)); do
  status=PASS
  if [[ "${stage_codes[$i]}" -ne 0 ]]; then
    status=FAIL
  fi
  printf '%-8s %-4s exit=%s\n' "${stage_names[$i]}" "$status" "${stage_codes[$i]}"
done
printf 'Full check logs: %s\n' "$CHECK_RUN_LOG_DIR"
if [[ "$failures" -gt 0 ]]; then
  printf 'RESULT FAIL (%s/%s stages failed)\n' "$failures" "${#stage_names[@]}"
  exit 1
fi
printf 'RESULT PASS (%s/%s stages passed)\n' "${#stage_names[@]}" "${#stage_names[@]}"
