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

case "$command" in
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

if find "$TMP_ROOT/tmp" -mindepth 1 -print -quit | grep -q .; then
  echo "race self-test left temporary diagnostics behind" >&2
  find "$TMP_ROOT/tmp" -mindepth 1 -print >&2
  exit 1
fi

echo "bench race self-test passed"
