#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/a3s-box-bench-self-test.XXXXXX")"
trap 'rm -rf -- "$TMP_ROOT"' EXIT
mkdir -p "$TMP_ROOT/tmp"

STUB="$TMP_ROOT/a3s-box"
STATE="$TMP_ROOT/boxes"

cat >"$STUB" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

state="${A3S_BOX_STUB_STATE:?}"
command="${1:-}"
shift || true

if [ -n "${A3S_BOX_STUB_COMMAND_LOG:-}" ]; then
  printf '%s %s\n' "$command" "$*" >>"$A3S_BOX_STUB_COMMAND_LOG"
fi

case "$command" in
  load|images|pull|volume|snapshot|--version)
    ;;
  ps)
    if [ "${2:-}" = "--format" ] && [ -f "$state" ]; then
      cat "$state"
    fi
    ;;
  run)
    name=""
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --name)
          name="${2:-}"
          shift 2
          ;;
        *) shift ;;
      esac
    done
    if [ "${A3S_BOX_STUB_FAIL_LAUNCHES:-0}" = "1" ]; then
      echo "synthetic launch failure for $name" >&2
      exit 42
    fi
    printf '%s\n' "$name" >>"$state"
    ;;
  rm)
    name="${2:-}"
    if [ -f "$state" ]; then
      awk -v name="$name" '$0 != name' "$state" >"$state.next"
      mv "$state.next" "$state"
    fi
    ;;
  *)
    echo "unexpected stub command: $command" >&2
    exit 64
    ;;
esac
EOF
chmod +x "$STUB"

run_race() {
  A3S_BOX="$STUB" \
    A3S_BOX_STUB_STATE="$STATE" \
    IMAGE=synthetic:latest \
    RACE=3 \
    RACE_WORKLOAD_SECS=60 \
    TMPDIR="$TMP_ROOT/tmp" \
    "$REPO_ROOT/bench/bench.sh" race 2>&1
}

run_leak() {
  A3S_BOX="$STUB" \
    A3S_BOX_STUB_STATE="$STATE" \
    IMAGE=synthetic:latest \
    CHURN=3 \
    TMPDIR="$TMP_ROOT/tmp" \
    "$REPO_ROOT/bench/bench.sh" leak 2>&1
}

set +e
leak_failure_output="$(A3S_BOX_STUB_FAIL_LAUNCHES=1 run_leak)"
leak_failure_status=$?
set -e
if [ "$leak_failure_status" -eq 0 ]; then
  echo "leak gate accepted failed churn runs" >&2
  exit 1
fi
grep -q 'FAIL: churn run 1 exited unsuccessfully' <<<"$leak_failure_output"
grep -q 'FAIL: 3/3 churn runs failed' <<<"$leak_failure_output"

set +e
failure_output="$(A3S_BOX_STUB_FAIL_LAUNCHES=1 run_race)"
failure_status=$?
set -e
if [ "$failure_status" -eq 0 ]; then
  echo "race gate accepted failed launches" >&2
  exit 1
fi
grep -q 'launched: 0/3 detached boxes reported success' <<<"$failure_output"
grep -q 'FAIL: 3/3 detached launches failed' <<<"$failure_output"

: >"$STATE"
success_output="$(run_race)"
grep -q 'launched: 3/3 detached boxes reported success' <<<"$success_output"
grep -q 'persisted: 3 entries .* (expected 3)' <<<"$success_output"
grep -q 'PASS: every required launch persisted' <<<"$success_output"
test ! -s "$STATE"

PNPM_COMMAND_LOG="$TMP_ROOT/pnpm-commands.log"
: >"$PNPM_COMMAND_LOG"
A3S_BOX="$STUB" \
  A3S_BOX_STUB_STATE="$STATE" \
  A3S_BOX_STUB_COMMAND_LOG="$PNPM_COMMAND_LOG" \
  PNPM_PROJECT="$REPO_ROOT/bench/fixtures/pnpm" \
  PNPM_IMAGE=synthetic:latest \
  PNPM_RUNS=1 \
  PNPM_NODE_MODULES=both \
  PNPM_LOG_DIR="$TMP_ROOT/pnpm-logs" \
  "$REPO_ROOT/bench/bench.sh" pnpm >/dev/null

project_cleanup_count=$(grep -c 'rm -rf node_modules' "$PNPM_COMMAND_LOG")
if [ "$project_cleanup_count" -ne 3 ]; then
  echo "pnpm self-test recorded $project_cleanup_count project cleanups; expected 3" >&2
  exit 1
fi
tmpfs_cleanup_count=$(grep -c 'find node_modules -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +' "$PNPM_COMMAND_LOG")
if [ "$tmpfs_cleanup_count" -ne 2 ]; then
  echo "pnpm self-test recorded $tmpfs_cleanup_count tmpfs cleanups; expected 2" >&2
  exit 1
fi
if grep -- '--tmpfs /work/node_modules' "$PNPM_COMMAND_LOG" | grep -q 'rm -rf node_modules'; then
  echo "pnpm self-test attempted to unlink a tmpfs mount point" >&2
  exit 1
fi

HOST_COMMAND_LOG="$TMP_ROOT/host-commands.log"
HOST_EVIDENCE="$TMP_ROOT/host-evidence"
OFFLINE_TAR="$TMP_ROOT/alpine.tar"
: >"$STATE"
: >"$HOST_COMMAND_LOG"
: >"$OFFLINE_TAR"
A3S_BOX="$STUB" \
  A3S_BOX_STUB_STATE="$STATE" \
  A3S_BOX_STUB_COMMAND_LOG="$HOST_COMMAND_LOG" \
  A3S_HOME="$TMP_ROOT/home" \
  A3S_BOX_TEST_ALPINE_TAR="$OFFLINE_TAR" \
  A3S_BOX_SMOKE_IMAGE_TAR="$OFFLINE_TAR" \
  IMAGE=synthetic:latest \
  CHURN=1 \
  RACE=1 \
  RACE_WORKLOAD_SECS=60 \
  TMPDIR="$TMP_ROOT/tmp" \
  "$REPO_ROOT/scripts/host-integration-smoke.sh" \
    --no-pure --soak --soak-duration 0 --soak-iterations 1 \
    --soak-output "$HOST_EVIDENCE" >/dev/null

load_count=$(grep -c '^load --input .* --tag synthetic:latest$' "$HOST_COMMAND_LOG")
if [ "$load_count" -ne 2 ]; then
  echo "host soak runner seeded the offline benchmark image $load_count times; expected 2" >&2
  exit 1
fi
grep -q '^result=pass$' "$HOST_EVIDENCE/summary.txt"
grep -q 'PASS: host soak evidence verified' "$HOST_EVIDENCE/verify.out"

if find "$TMP_ROOT/tmp" -mindepth 1 -print -quit | grep -q .; then
  echo "race self-test left temporary diagnostics behind" >&2
  find "$TMP_ROOT/tmp" -mindepth 1 -print >&2
  exit 1
fi

echo "bench leak/race, pnpm cleanup, and offline soak seeding self-test passed"
