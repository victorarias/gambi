#!/usr/bin/env bash
set -euo pipefail

PROFILE="${1:-ci}"
STRESS_LOOPS="${GAMBI_STRESS_LOOPS:-5}"
STRESS_MATRIX="${GAMBI_STRESS_MATRIX:-1}"

run_reliability_test() {
  local test_name="$1"
  shift
  echo "[harness] reliability ${test_name}"
  env RUST_LOG=error "$@" cargo test --test reliability_harness "${test_name}" -- --nocapture --test-threads=1
}

run_reliability_matrix() {
  run_reliability_test \
    gambi_handles_parallel_mixed_operations_without_errors \
    GAMBI_RELIABILITY_MIXED_JOBS="${GAMBI_RELIABILITY_MIXED_JOBS:-72}" \
    GAMBI_RELIABILITY_MIXED_ROUNDS="${GAMBI_RELIABILITY_MIXED_ROUNDS:-1}"

  run_reliability_test \
    gambi_stays_healthy_during_remote_bounce_under_load \
    GAMBI_RELIABILITY_BOUNCE_CYCLES="${GAMBI_RELIABILITY_BOUNCE_CYCLES:-12}" \
    GAMBI_RELIABILITY_BOUNCE_EVERY="${GAMBI_RELIABILITY_BOUNCE_EVERY:-3}" \
    GAMBI_RELIABILITY_BOUNCE_WORKERS="${GAMBI_RELIABILITY_BOUNCE_WORKERS:-6}" \
    GAMBI_RELIABILITY_RETRY_ATTEMPTS="${GAMBI_RELIABILITY_RETRY_ATTEMPTS:-8}" \
    GAMBI_RELIABILITY_RETRY_BACKOFF_MS="${GAMBI_RELIABILITY_RETRY_BACKOFF_MS:-125}" \
    GAMBI_RELIABILITY_BOUNCE_PAUSE_MS="${GAMBI_RELIABILITY_BOUNCE_PAUSE_MS:-120}"
}

run_heavy_matrix_once() {
  run_reliability_test \
    gambi_handles_parallel_mixed_operations_without_errors \
    GAMBI_RELIABILITY_MIXED_JOBS="${GAMBI_RELIABILITY_HEAVY_MIXED_JOBS:-192}" \
    GAMBI_RELIABILITY_MIXED_ROUNDS="${GAMBI_RELIABILITY_HEAVY_MIXED_ROUNDS:-2}"

  run_reliability_test \
    gambi_stays_healthy_during_remote_bounce_under_load \
    GAMBI_RELIABILITY_BOUNCE_CYCLES="${GAMBI_RELIABILITY_HEAVY_BOUNCE_CYCLES:-20}" \
    GAMBI_RELIABILITY_BOUNCE_EVERY="${GAMBI_RELIABILITY_HEAVY_BOUNCE_EVERY:-2}" \
    GAMBI_RELIABILITY_BOUNCE_WORKERS="${GAMBI_RELIABILITY_HEAVY_BOUNCE_WORKERS:-10}" \
    GAMBI_RELIABILITY_RETRY_ATTEMPTS="${GAMBI_RELIABILITY_HEAVY_RETRY_ATTEMPTS:-10}" \
    GAMBI_RELIABILITY_RETRY_BACKOFF_MS="${GAMBI_RELIABILITY_HEAVY_RETRY_BACKOFF_MS:-100}" \
    GAMBI_RELIABILITY_BOUNCE_PAUSE_MS="${GAMBI_RELIABILITY_HEAVY_BOUNCE_PAUSE_MS:-120}"
}

run_base() {
  echo "[harness] fmt"
  cargo fmt --all --check

  echo "[harness] clippy"
  cargo clippy --all-targets --all-features -- -D warnings

  echo "[harness] test"
  RUST_LOG=error cargo test
}

run_ui() {
  if [[ "${GAMBI_SKIP_UI:-0}" == "1" ]]; then
    echo "[harness] ui skipped (GAMBI_SKIP_UI=1)"
    return
  fi

  echo "[harness] ui"
  ./scripts/test-ui.sh
}

case "${PROFILE}" in
  smoke)
    echo "[harness] smoke"
    RUST_LOG=error cargo test --test execution_engine
    RUST_LOG=error cargo test --test execute_multi_upstream
    RUST_LOG=error cargo test --test upstream_passthrough
    RUST_LOG=error cargo test split_namespaced_roundtrip_property
    ;;
  ci)
    echo "[harness] ci"
    run_base
    run_ui
    ;;
  full)
    echo "[harness] full"
    run_base
    run_ui

    for i in $(seq 1 "${STRESS_LOOPS}"); do
      echo "[harness] stress iteration ${i}/${STRESS_LOOPS}"
      run_reliability_matrix
    done

    if [[ "${STRESS_MATRIX}" == "1" ]]; then
      echo "[harness] heavy matrix"
      run_heavy_matrix_once
    fi
    ;;
  soak)
    echo "[harness] soak"
    run_base
    run_ui

    for i in $(seq 1 "${STRESS_LOOPS}"); do
      echo "[harness] soak iteration ${i}/${STRESS_LOOPS}"
      run_heavy_matrix_once
    done
    ;;
  ui)
    echo "[harness] ui"
    run_ui
    ;;
  *)
    echo "usage: scripts/test-harness.sh [smoke|ci|full|soak|ui]" >&2
    exit 1
    ;;
esac
