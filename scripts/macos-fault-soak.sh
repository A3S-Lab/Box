#!/usr/bin/env bash
# macOS/HVF fault-injection soak with isolated state and machine-readable evidence.
set -euo pipefail

A3S_BOX="${A3S_BOX:-a3s-box}"
IMAGE="${IMAGE:-alpine:latest}"
DURATION_SECS=259200
SAMPLE_INTERVAL_SECS=300
FAULT_INTERVAL_SECS=900
MIN_FREE_GIB=100
MAX_DISK_PERCENT=80
MIN_OPEN_FILES=4096
OUTPUT=""
PREFLIGHT_ONLY=0
KEEP_HOME=0

usage() {
    cat <<'EOF'
Usage: scripts/macos-fault-soak.sh [options]

Options:
  --duration SECS          Run duration (default: 259200 / 72 hours)
  --sample-interval SECS   Resource sampling interval (default: 300)
  --fault-interval SECS    Fault injection interval (default: 900)
  --output DIR             Evidence directory
  --image IMAGE            OCI image (default: alpine:latest)
  --min-free-gib N         Admission threshold (default: 100)
  --max-disk-percent N     Stop threshold (default: 80)
  --min-open-files N       Required file descriptor limit (default: 4096)
  --preflight-only         Check admission without creating workloads
  --keep-home              Preserve the isolated A3S_HOME after the run
  -h, --help               Show this help

The runner never uses ~/.a3s. It creates an isolated A3S_HOME below the evidence
directory, injects faults only into boxes whose names begin with its unique run
prefix, and records resource samples, operation results, recovery assertions,
and a final summary.
EOF
}

die() { echo "ERROR: $*" >&2; exit 1; }
is_uint() { [[ "$1" =~ ^[0-9]+$ ]]; }

while [ "$#" -gt 0 ]; do
    case "$1" in
        --duration) DURATION_SECS="${2:?missing duration}"; shift 2 ;;
        --sample-interval) SAMPLE_INTERVAL_SECS="${2:?missing interval}"; shift 2 ;;
        --fault-interval) FAULT_INTERVAL_SECS="${2:?missing interval}"; shift 2 ;;
        --output) OUTPUT="${2:?missing output}"; shift 2 ;;
        --image) IMAGE="${2:?missing image}"; shift 2 ;;
        --min-free-gib) MIN_FREE_GIB="${2:?missing threshold}"; shift 2 ;;
        --max-disk-percent) MAX_DISK_PERCENT="${2:?missing threshold}"; shift 2 ;;
        --min-open-files) MIN_OPEN_FILES="${2:?missing threshold}"; shift 2 ;;
        --preflight-only) PREFLIGHT_ONLY=1; shift ;;
        --keep-home) KEEP_HOME=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown argument: $1" ;;
    esac
done

for value in "$DURATION_SECS" "$SAMPLE_INTERVAL_SECS" "$FAULT_INTERVAL_SECS" \
    "$MIN_FREE_GIB" "$MAX_DISK_PERCENT" "$MIN_OPEN_FILES"; do
    is_uint "$value" || die "numeric options must be non-negative integers"
done
[ "$DURATION_SECS" -gt 0 ] || die "duration must be positive"
[ "$SAMPLE_INTERVAL_SECS" -gt 0 ] || die "sample interval must be positive"
[ "$FAULT_INTERVAL_SECS" -gt 0 ] || die "fault interval must be positive"

[ "$(uname -s)" = Darwin ] || die "this runner requires macOS"
[ "$(uname -m)" = arm64 ] || die "this runner requires Apple Silicon"
command -v "$A3S_BOX" >/dev/null 2>&1 || die "a3s-box not found: $A3S_BOX"
command -v jq >/dev/null 2>&1 || die "jq is required"

RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-$$"
OUTPUT="${OUTPUT:-$(pwd)/target/a3s-box-macos-fault-soak/$RUN_ID}"
mkdir -p "$OUTPUT"
OUTPUT="$(cd "$OUTPUT" && pwd)"
export A3S_HOME="$OUTPUT/a3s-home"
PREFIX="fault-soak-$RUN_ID"
SAMPLES="$OUTPUT/resource-samples.tsv"
OPERATIONS="$OUTPUT/operations.tsv"
SUMMARY="$OUTPUT/summary.txt"
START_EPOCH="$(date +%s)"
FAILURES=0
EXPECTED_FAULTS=0
OPERATIONS_TOTAL=0
LAST_SAMPLE_EPOCH=0
LAST_FAULT_EPOCH=0

disk_percent() { df -Pk "$OUTPUT" | awk 'NR==2 {gsub(/%/, "", $5); print $5}'; }
free_gib() { df -Pk "$OUTPUT" | awk 'NR==2 {printf "%d", $4 / 1024 / 1024}'; }
shim_count() { { pgrep -f "$A3S_HOME/boxes/" 2>/dev/null || true; } | wc -l | tr -d ' '; }
box_dir_count() { find "$A3S_HOME/boxes" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l | tr -d ' '; }
socket_dir_count() {
    { find "${TMPDIR:-/tmp}/a3s-box-sockets" -mindepth 1 -maxdepth 1 -type d 2>/dev/null || true; } |
        while IFS= read -r dir; do
            pgrep -f "$dir" >/dev/null 2>&1 && echo "$dir"
        done | wc -l | tr -d ' '
}
home_bytes() { du -sk "$A3S_HOME" 2>/dev/null | awk '{print $1 * 1024}' || echo 0; }

write_sample() {
    local phase="$1" now
    now="$(date +%s)"
    if [ ! -f "$SAMPLES" ]; then
        printf 'timestamp\tepoch\tphase\tshims\tbox_dirs\tsocket_dirs\ta3s_home_bytes\tdisk_percent\tfree_gib\n' >"$SAMPLES"
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$now" "$phase" "$(shim_count)" \
        "$(box_dir_count)" "$(socket_dir_count)" "$(home_bytes)" \
        "$(disk_percent)" "$(free_gib)" >>"$SAMPLES"
    LAST_SAMPLE_EPOCH="$now"
}

record_operation() {
    local kind="$1" result="$2" detail="$3"
    if [ ! -f "$OPERATIONS" ]; then
        printf 'timestamp\tkind\tresult\tdetail\n' >"$OPERATIONS"
    fi
    printf '%s\t%s\t%s\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$kind" "$result" \
        "$(printf '%s' "$detail" | tr '\t\r\n' '   ')" >>"$OPERATIONS"
    OPERATIONS_TOTAL=$((OPERATIONS_TOTAL + 1))
    [ "$result" = pass ] || FAILURES=$((FAILURES + 1))
}

cleanup_boxes() {
    local name
    while IFS= read -r name; do
        case "$name" in
            "$PREFIX"-*) "$A3S_BOX" rm -f "$name" >/dev/null 2>&1 || true ;;
        esac
    done < <("$A3S_BOX" ps -a --format '{{.Names}}' 2>/dev/null || true)
}

finish() {
    local rc="$1" end duration final_shims final_dirs result
    set +e
    cleanup_boxes
    sleep 2
    write_sample final
    final_shims="$(shim_count)"
    final_dirs="$(box_dir_count)"
    [ "$final_shims" -eq 0 ] || FAILURES=$((FAILURES + 1))
    [ "$final_dirs" -eq 0 ] || FAILURES=$((FAILURES + 1))
    end="$(date +%s)"; duration=$((end - START_EPOCH))
    result=pass
    [ "$rc" -eq 0 ] && [ "$FAILURES" -eq 0 ] || result=fail
    {
        echo "result=$result"
        echo "duration_secs=$duration"
        echo "operations=$OPERATIONS_TOTAL"
        echo "expected_faults=$EXPECTED_FAULTS"
        echo "failures=$FAILURES"
        echo "final_shims=$final_shims"
        echo "final_box_dirs=$final_dirs"
        echo "evidence_dir=$OUTPUT"
    } >"$SUMMARY"
    if [ "$KEEP_HOME" -eq 0 ] && [ "$final_shims" -eq 0 ]; then rm -rf "$A3S_HOME"; fi
    echo "macOS fault soak: $result (evidence: $OUTPUT)"
    [ "$result" = pass ]
}
trap 'finish "$?"' EXIT INT TERM

OPEN_FILES="$(ulimit -n)"
CURRENT_FREE_GIB="$(free_gib)"
CURRENT_DISK_PERCENT="$(disk_percent)"
{
    echo "run_id=$RUN_ID"
    echo "started_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "git_sha=$(git rev-parse HEAD 2>/dev/null || true)"
    echo "a3s_box=$($A3S_BOX version 2>&1 | head -1)"
    echo "image=$IMAGE"
    echo "duration_secs=$DURATION_SECS"
    echo "sample_interval_secs=$SAMPLE_INTERVAL_SECS"
    echo "fault_interval_secs=$FAULT_INTERVAL_SECS"
    echo "open_files=$OPEN_FILES"
    echo "free_gib=$CURRENT_FREE_GIB"
    echo "disk_percent=$CURRENT_DISK_PERCENT"
    uname -a
    sw_vers
} >"$OUTPUT/metadata.txt"

[ "$OPEN_FILES" -ge "$MIN_OPEN_FILES" ] || die "open-file limit $OPEN_FILES is below $MIN_OPEN_FILES"
[ "$CURRENT_FREE_GIB" -ge "$MIN_FREE_GIB" ] || die "free disk ${CURRENT_FREE_GIB} GiB is below ${MIN_FREE_GIB} GiB"
[ "$CURRENT_DISK_PERCENT" -le "$MAX_DISK_PERCENT" ] || die "disk usage ${CURRENT_DISK_PERCENT}% exceeds ${MAX_DISK_PERCENT}%"
[ "$(sysctl -n kern.hv_support 2>/dev/null || echo 0)" = 1 ] || die "Hypervisor.framework is unavailable"

if [ "$PREFLIGHT_ONLY" -eq 1 ]; then
    trap - EXIT INT TERM
    echo "preflight=pass" >"$SUMMARY"
    echo "macOS fault-soak preflight passed: $OUTPUT"
    exit 0
fi

mkdir -p "$A3S_HOME"
write_sample start
"$A3S_BOX" pull "$IMAGE" >"$OUTPUT/pull.log" 2>&1

run_normal_operation() {
    if "$A3S_BOX" run --rm "$IMAGE" -- sh -c 'test "$(printf recovery)" = recovery' >/dev/null 2>&1; then
        record_operation lifecycle pass run-rm
    else
        record_operation lifecycle fail run-rm
    fi
}

inject_shim_kill() {
    local name="$PREFIX-shim-$OPERATIONS_TOTAL" inspect id pid
    if ! "$A3S_BOX" run -d --name "$name" "$IMAGE" -- sleep 300 >/dev/null 2>&1; then
        record_operation shim-kill fail launch; return
    fi
    inspect="$($A3S_BOX inspect "$name" 2>/dev/null || true)"
    id="$(printf '%s' "$inspect" | jq -r 'if type=="array" then .[0] else . end | .Id // .ID // .id // empty' 2>/dev/null)"
    pid="$(pgrep -f "\"box_id\":\"$id\"" 2>/dev/null | head -1 || true)"
    if [ -z "$id" ] || [ -z "$pid" ]; then
        "$A3S_BOX" rm -f "$name" >/dev/null 2>&1 || true
        record_operation shim-kill fail "shim-not-found id=$id"; return
    fi
    kill -9 "$pid" 2>/dev/null || true
    EXPECTED_FAULTS=$((EXPECTED_FAULTS + 1))
    sleep 2
    "$A3S_BOX" rm -f "$name" >/dev/null 2>&1 || true
    if "$A3S_BOX" ps -a >/dev/null 2>&1 && ! kill -0 "$pid" 2>/dev/null; then
        record_operation shim-kill pass "pid=$pid"
    else
        record_operation shim-kill fail "recovery-failed pid=$pid"
    fi
}

inject_cli_kill() {
    local name="$PREFIX-cli-$OPERATIONS_TOTAL" pid
    "$A3S_BOX" run --name "$name" "$IMAGE" -- sleep 300 >/dev/null 2>&1 &
    pid=$!
    sleep 1
    kill -9 "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    EXPECTED_FAULTS=$((EXPECTED_FAULTS + 1))
    "$A3S_BOX" rm -f "$name" >/dev/null 2>&1 || true
    if "$A3S_BOX" ps -a >/dev/null 2>&1; then
        record_operation cli-kill pass "pid=$pid"
    else
        record_operation cli-kill fail "state-unreadable pid=$pid"
    fi
}

END_EPOCH=$((START_EPOCH + DURATION_SECS))
FAULT_KIND=0
while [ "$(date +%s)" -lt "$END_EPOCH" ]; do
    now="$(date +%s)"
    if [ $((now - LAST_SAMPLE_EPOCH)) -ge "$SAMPLE_INTERVAL_SECS" ]; then
        write_sample periodic
        [ "$(disk_percent)" -le "$MAX_DISK_PERCENT" ] || die "disk stop condition fired"
    fi
    run_normal_operation
    now="$(date +%s)"
    if [ $((now - LAST_FAULT_EPOCH)) -ge "$FAULT_INTERVAL_SECS" ]; then
        if [ "$FAULT_KIND" -eq 0 ]; then inject_shim_kill; FAULT_KIND=1; else inject_cli_kill; FAULT_KIND=0; fi
        LAST_FAULT_EPOCH="$now"
        write_sample post-fault
    fi
done
