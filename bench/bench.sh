#!/usr/bin/env bash
# Reproducible a3s-box benchmark + leak harness.
#
# Makes the perf claims (cold boot, snapshot-fork, warm-pool acquire) and the
# leak-free claim INDEPENDENTLY REPRODUCIBLE: it drives the real `a3s-box` CLI
# end-to-end and reports wall-clock latencies + a hard leak assertion, instead
# of quoting numbers from prose. Run it on a Linux host with /dev/kvm (the only
# place real microVMs boot).
#
# Usage:
#   bench/bench.sh [all|cold|warm|fork|leak|race|pnpm]   (default: all)
# Env:
#   A3S_BOX   path to the a3s-box binary           (default: a3s-box on PATH)
#   IMAGE     OCI image to benchmark                (default: alpine:latest)
#   RUNS      samples per latency benchmark         (default: 20)
#   POOL_SIZE warm-pool / fork fill size            (default: 16)
#   CHURN     create/run/remove cycles for the leak test (default: 30)
#   RACE      concurrent `run -d` processes for the cross-process race (default: 8)
#   PNPM_PROJECT project dir with package.json + pnpm-lock.yaml (required for pnpm mode)
#   PNPM_IMAGE   Node image for pnpm mode             (default: node:22-alpine)
#   PNPM_VERSION pnpm version for corepack prepare    (default: 10.30.3)
#   PNPM_RUNS    pnpm install samples                 (default: 3)
#   PNPM_CACHE   1 uses --package-cache pnpm, 0 disables it (default: 1)
#   PNPM_CPUS    CPUs for pnpm boxes/containers       (default: 4)
#   PNPM_MEMORY  memory for pnpm boxes/containers     (default: 4g)
#   PNPM_DOCKER  1 compares Docker cold/hot baselines, 0 skips (default: 1)
#   PNPM_RESET_A3S_CACHE 1 removes a3s-cache-pnpm before cold A3S samples (default: 0)
#
# Exit code is non-zero if the leak assertion fails, so it is CI-gateable
# (wire it into the self-hosted KVM job — see docs/ci-kvm-runner.md).
set -u

A3S_BOX="${A3S_BOX:-a3s-box}"
IMAGE="${IMAGE:-alpine:latest}"
RUNS="${RUNS:-20}"
POOL_SIZE="${POOL_SIZE:-16}"
CHURN="${CHURN:-30}"
RACE="${RACE:-8}"
PNPM_PROJECT="${PNPM_PROJECT:-}"
PNPM_IMAGE="${PNPM_IMAGE:-node:22-alpine}"
PNPM_VERSION="${PNPM_VERSION:-10.30.3}"
PNPM_RUNS="${PNPM_RUNS:-3}"
PNPM_CACHE="${PNPM_CACHE:-1}"
PNPM_CPUS="${PNPM_CPUS:-4}"
PNPM_MEMORY="${PNPM_MEMORY:-4g}"
PNPM_DOCKER="${PNPM_DOCKER:-1}"
PNPM_TMPFS_SIZE="${PNPM_TMPFS_SIZE:-4g}"
PNPM_NODE_MODULES="${PNPM_NODE_MODULES:-both}"
PNPM_LOG_DIR="${PNPM_LOG_DIR:-/tmp/a3s-bench-pnpm}"
PNPM_DOCKER_STORE_VOLUME="${PNPM_DOCKER_STORE_VOLUME:-a3s-bench-pnpm-store}"
PNPM_A3S_CACHE_VOLUME="${PNPM_A3S_CACHE_VOLUME:-a3s-cache-pnpm}"
PNPM_RESET_A3S_CACHE="${PNPM_RESET_A3S_CACHE:-0}"
MODE="${1:-all}"

now_ms() {
  local ts
  ts=$(date +%s%3N 2>/dev/null || true)
  case "$ts" in
    ''|*[!0-9]*) python3 -c 'import time;print(int(time.time()*1000))' ;;
    *) echo "$ts" ;;
  esac
}

# Percentile of a space-separated list of integers. $1=list $2=pct(0-100)
pct() {
  local nums; nums=$(printf '%s\n' $1 | sort -n)
  local count; count=$(printf '%s\n' $nums | wc -l | tr -d ' ')
  [ "$count" -eq 0 ] && { echo 0; return; }
  local idx=$(( (count * $2 + 99) / 100 ))
  [ "$idx" -lt 1 ] && idx=1
  printf '%s\n' $nums | sed -n "${idx}p"
}

ratio() {
  awk -v a="$1" -v b="$2" 'BEGIN { if (b <= 0) print "n/a"; else printf "%.2fx", a / b }'
}

require_kvm() {
  if [ ! -e /dev/kvm ]; then
    echo "WARNING: /dev/kvm not present — boot benchmarks measure a degraded/failed path." >&2
  fi
  command -v "$A3S_BOX" >/dev/null 2>&1 || { echo "ERROR: a3s-box not found ($A3S_BOX)"; exit 2; }
}

# Count host-side resources that a leak would grow.
shim_count() {
  local count
  count=$(pgrep -xc 'a3s-box-shim' 2>/dev/null || pgrep -fc 'a3s-box-shim' 2>/dev/null || true)
  echo "${count:-0}"
}
mount_count() { mount 2>/dev/null | awk '/\/\.a3s\/boxes|\/a3s\/boxes/ { n++ } END { print n + 0 }'; }
boxdir_count() { ls -1 "${HOME}/.a3s/boxes" 2>/dev/null | wc -l | tr -d ' '; }
fd_count() { ls -1 "/proc/$$/fd" 2>/dev/null | wc -l | tr -d ' '; }

bench_cold() {
  echo "## Cold boot ($RUNS runs, $IMAGE)"
  "$A3S_BOX" pull "$IMAGE" >/dev/null 2>&1 || true
  local samples=""
  for _ in $(seq 1 "$RUNS"); do
    local s e; s=$(now_ms)
    "$A3S_BOX" run --rm "$IMAGE" -- true >/dev/null 2>&1
    e=$(now_ms); samples="$samples $(( e - s ))"
  done
  echo "  p50=$(pct "$samples" 50)ms  p90=$(pct "$samples" 90)ms  min=$(pct "$samples" 1)ms"
}

bench_warm() {
  echo "## Warm-pool acquire ($RUNS runs, pool size $POOL_SIZE)"
  local sock=/tmp/a3s-bench-pool.sock
  "$A3S_BOX" pool start --image "$IMAGE" --size "$POOL_SIZE" --socket "$sock" >/tmp/a3s-bench-pool.log 2>&1 &
  local daemon=$!
  # Wait for the pool to be ready (socket appears + first acquire succeeds).
  for _ in $(seq 1 60); do [ -S "$sock" ] && break; sleep 1; done
  sleep 3
  local samples=""
  for _ in $(seq 1 "$RUNS"); do
    local s e; s=$(now_ms)
    "$A3S_BOX" pool run --socket "$sock" -- true >/dev/null 2>&1
    e=$(now_ms); samples="$samples $(( e - s ))"
  done
  kill "$daemon" 2>/dev/null; wait "$daemon" 2>/dev/null
  echo "  p50=$(pct "$samples" 50)ms  p90=$(pct "$samples" 90)ms  min=$(pct "$samples" 1)ms"
}

bench_fork() {
  echo "## Snapshot-fork pool fill ($POOL_SIZE VMs, cold-boot vs CoW restore)"
  local sock=/tmp/a3s-bench-fork.sock
  for mode in "" "--snapshot-fork"; do
    local label="cold-fill"; [ -n "$mode" ] && label="snapshot-fork"
    local s e; s=$(now_ms)
    # shellcheck disable=SC2086
    "$A3S_BOX" pool start --image "$IMAGE" --size "$POOL_SIZE" --socket "$sock" $mode >/tmp/a3s-bench-fork.log 2>&1 &
    local daemon=$!
    for _ in $(seq 1 120); do [ -S "$sock" ] && "$A3S_BOX" pool run --socket "$sock" -- true >/dev/null 2>&1 && break; sleep 1; done
    e=$(now_ms)
    kill "$daemon" 2>/dev/null; wait "$daemon" 2>/dev/null
    local total=$(( e - s ))
    echo "  $label: fill-to-$POOL_SIZE ${total}ms (~$(( total / POOL_SIZE ))ms amortized)"
    sleep 2
  done
}

bench_leak() {
  echo "## Leak assertion ($CHURN create/run/remove cycles)"
  local b_shim b_mount b_dir
  b_shim=$(shim_count); b_mount=$(mount_count); b_dir=$(boxdir_count)
  echo "  baseline: shims=$b_shim mounts=$b_mount box-dirs=$b_dir"
  for _ in $(seq 1 "$CHURN"); do
    "$A3S_BOX" run --rm "$IMAGE" -- true >/dev/null 2>&1
  done
  sleep 3
  local a_shim a_mount a_dir
  a_shim=$(shim_count); a_mount=$(mount_count); a_dir=$(boxdir_count)
  echo "  after:    shims=$a_shim mounts=$a_mount box-dirs=$a_dir"
  local leak=0
  [ "$a_shim" -gt "$b_shim" ]  && { echo "  LEAK: $(( a_shim - b_shim )) orphan shim(s)"; leak=1; }
  [ "$a_mount" -gt "$b_mount" ] && { echo "  LEAK: $(( a_mount - b_mount )) leaked overlay mount(s)"; leak=1; }
  [ "$a_dir" -gt "$b_dir" ]    && { echo "  LEAK: $(( a_dir - b_dir )) leaked box dir(s)"; leak=1; }
  if [ "$leak" -eq 0 ]; then echo "  PASS: no orphan shims / mounts / box dirs after churn"; else echo "  FAIL: resource leak detected"; fi
  return "$leak"
}

# Cross-process lost-update + JSON-integrity assertion.
#
# Every `a3s-box run -d` registers its box by load-modify-saving the SHARED
# boxes.json. In production multiple CLIs (plus the monitor and CRI) do this
# concurrently, so the write path is guarded by a cross-process advisory lock
# (flock on boxes.json.lock). The unit tests only race in-process threads and
# therefore CANNOT catch a broken or missing flock. This boots RACE real
# microVMs at once and asserts that (a) boxes.json still parses afterward and
# (b) every launch that reported success actually persisted — a clobbered
# load-modify-save would silently drop entries (the classic lost update).
bench_race() {
  local N="$RACE"
  local tag="race-$$"
  echo "## Cross-process race ($N concurrent run -d -> boxes.json)"

  # Baseline must load cleanly, so a failure below is attributable to the race.
  if ! "$A3S_BOX" ps -a >/dev/null 2>&1; then
    echo "  FAIL: boxes.json does not load before the race"; return 1
  fi

  local pids="" p
  for i in $(seq 1 "$N"); do
    "$A3S_BOX" run -d --name "$tag-$i" "$IMAGE" -- sleep 30 >/dev/null 2>&1 &
    pids="$pids $!"
  done
  local ok=0
  for p in $pids; do wait "$p" && ok=$(( ok + 1 )); done
  echo "  launched: $ok/$N detached boxes reported success"

  local rc=0
  # The CLI re-reads boxes.json here; a torn write makes this fail or quarantine.
  if ! "$A3S_BOX" ps -a >/dev/null 2>&1; then
    echo "  FAIL: boxes.json is unreadable after the race (torn write)"
    rc=1
  else
    local persisted
    persisted=$("$A3S_BOX" ps -a --format '{{.Names}}' 2>/dev/null | grep -c "^$tag-")
    echo "  persisted: $persisted entries named $tag-* (expected $ok)"
    if [ "$persisted" -ne "$ok" ]; then
      echo "  FAIL: lost update — $ok launches succeeded but only $persisted persisted"
      rc=1
    else
      echo "  PASS: every successful launch persisted (no lost update, JSON intact)"
    fi
  fi

  # Cleanup: remove every race box and verify none linger (the test must not leak).
  for i in $(seq 1 "$N"); do "$A3S_BOX" rm -f "$tag-$i" >/dev/null 2>&1; done
  sleep 2
  local left
  left=$("$A3S_BOX" ps -a --format '{{.Names}}' 2>/dev/null | grep -c "^$tag-")
  [ "$left" -ne 0 ] && { echo "  FAIL: $left race box(es) survived cleanup"; rc=1; }
  return "$rc"
}

bench_pnpm() {
  echo "## pnpm install benchmark ($PNPM_RUNS runs, image=$PNPM_IMAGE)"
  if [ -z "$PNPM_PROJECT" ]; then
    echo "  ERROR: set PNPM_PROJECT to a project directory with package.json and pnpm-lock.yaml" >&2
    return 2
  fi
  if [ ! -f "$PNPM_PROJECT/package.json" ] || [ ! -f "$PNPM_PROJECT/pnpm-lock.yaml" ]; then
    echo "  ERROR: PNPM_PROJECT must contain package.json and pnpm-lock.yaml: $PNPM_PROJECT" >&2
    return 2
  fi
  case "$PNPM_NODE_MODULES" in
    project|tmpfs|both) ;;
    *) echo "  ERROR: PNPM_NODE_MODULES must be project, tmpfs, or both" >&2; return 2 ;;
  esac

  local project
  project=$(cd "$PNPM_PROJECT" && pwd)
  mkdir -p "$PNPM_LOG_DIR"
  "$A3S_BOX" pull "$PNPM_IMAGE" >/dev/null 2>&1 || true

  echo "  config: project=$project cpus=$PNPM_CPUS memory=$PNPM_MEMORY package-cache=$PNPM_CACHE node_modules=$PNPM_NODE_MODULES reset-a3s-cache=$PNPM_RESET_A3S_CACHE"
  echo "  logs:   $PNPM_LOG_DIR"

  local prepare_cmd fetch_cmd offline_cmd full_cmd
  prepare_cmd="corepack enable && corepack prepare pnpm@$PNPM_VERSION --activate >/dev/null && pnpm --version >/dev/null"
  fetch_cmd="$prepare_cmd && rm -rf node_modules && pnpm fetch --frozen-lockfile --reporter append-only"
  offline_cmd="$prepare_cmd && rm -rf node_modules && pnpm install --offline --frozen-lockfile --ignore-scripts --reporter append-only"
  full_cmd="$prepare_cmd && rm -rf node_modules && pnpm install --frozen-lockfile --reporter append-only"

  run_a3s_pnpm() {
    local log_file="$1"; shift
    local guest_cmd="$1"; shift
    local cache_args=()
    [ "$PNPM_CACHE" = "1" ] && cache_args=(--package-cache pnpm)
    "$A3S_BOX" run --rm --cpus "$PNPM_CPUS" --memory "$PNPM_MEMORY" "${cache_args[@]}" "$@" "$PNPM_IMAGE" -- sh -lc "$guest_cmd" >"$log_file" 2>&1
  }

  run_docker_pnpm() {
    local log_file="$1"; shift
    local guest_cmd="$1"; shift
    docker run --rm --cpus "$PNPM_CPUS" --memory "$PNPM_MEMORY" \
      -v "$PNPM_DOCKER_STORE_VOLUME:/a3s-cache/pnpm" \
      -e npm_config_store_dir=/a3s-cache/pnpm/store \
      -e COREPACK_HOME=/a3s-cache/pnpm/corepack \
      "$@" "$PNPM_IMAGE" sh -lc "$guest_cmd" >"$log_file" 2>&1
  }

  MEASURED_SAMPLES=""
  measure_a3s_samples() {
    local label="$1"; shift
    local guest_cmd="$1"; shift
    local samples="" i s e status log_file
    for i in $(seq 1 "$PNPM_RUNS"); do
      log_file="$PNPM_LOG_DIR/a3s-$label-$i.log"
      if [ "$PNPM_RESET_A3S_CACHE" = "1" ] && [ "$PNPM_CACHE" = "1" ]; then
        case "$label" in
          toolchain|fetch|install-*) "$A3S_BOX" volume rm -f "$PNPM_A3S_CACHE_VOLUME" >/dev/null 2>&1 || true ;;
        esac
      fi
      s=$(now_ms)
      run_a3s_pnpm "$log_file" "$guest_cmd" "$@"
      status=$?
      e=$(now_ms); samples="$samples $(( e - s ))"
      if [ "$status" -ne 0 ]; then
        echo "  FAIL: A3S $label failed (see $log_file)" >&2
        tail -80 "$log_file" >&2
        return "$status"
      fi
    done
    MEASURED_SAMPLES="$samples"
  }

  measure_docker_samples() {
    local label="$1"; shift
    local guest_cmd="$1"; shift
    local samples="" i s e status log_file
    for i in $(seq 1 "$PNPM_RUNS"); do
      log_file="$PNPM_LOG_DIR/docker-$label-$i.log"
      case "$label" in
        cold-*) docker volume rm -f "$PNPM_DOCKER_STORE_VOLUME" >/dev/null 2>&1 || true ;;
      esac
      s=$(now_ms)
      run_docker_pnpm "$log_file" "$guest_cmd" "$@"
      status=$?
      e=$(now_ms); samples="$samples $(( e - s ))"
      if [ "$status" -ne 0 ]; then
        echo "  FAIL: Docker $label failed (see $log_file)" >&2
        tail -80 "$log_file" >&2
        return "$status"
      fi
    done
    MEASURED_SAMPLES="$samples"
  }

  local boot_samples="" toolchain_samples="" install_project_samples="" install_tmpfs_samples=""
  local fetch_samples="" offline_project_samples="" offline_tmpfs_samples=""
  for _ in $(seq 1 "$PNPM_RUNS"); do
    local s e

    s=$(now_ms)
    "$A3S_BOX" run --rm --cpus "$PNPM_CPUS" --memory "$PNPM_MEMORY" "$PNPM_IMAGE" -- true >/dev/null 2>&1
    e=$(now_ms); boot_samples="$boot_samples $(( e - s ))"
  done

  measure_a3s_samples "toolchain" "$prepare_cmd" || return $?
  toolchain_samples="$MEASURED_SAMPLES"

  if [ "$PNPM_CACHE" = "1" ]; then
    measure_a3s_samples "fetch" "$fetch_cmd" -v "$project:/work" -w /work || return $?
    fetch_samples="$MEASURED_SAMPLES"

    measure_a3s_samples "offline-project" "$offline_cmd" -v "$project:/work" -w /work || return $?
    offline_project_samples="$MEASURED_SAMPLES"

    if [ "$PNPM_NODE_MODULES" = "tmpfs" ] || [ "$PNPM_NODE_MODULES" = "both" ]; then
      measure_a3s_samples "offline-tmpfs" "$offline_cmd" -v "$project:/work" -w /work --tmpfs "/work/node_modules:size=$PNPM_TMPFS_SIZE" || return $?
      offline_tmpfs_samples="$MEASURED_SAMPLES"
    fi
  fi

  if [ "$PNPM_NODE_MODULES" = "project" ] || [ "$PNPM_NODE_MODULES" = "both" ]; then
    measure_a3s_samples "install-project" "$full_cmd" -v "$project:/work" -w /work || return $?
    install_project_samples="$MEASURED_SAMPLES"
  fi

  if [ "$PNPM_NODE_MODULES" = "tmpfs" ] || [ "$PNPM_NODE_MODULES" = "both" ]; then
    measure_a3s_samples "install-tmpfs" "$full_cmd" -v "$project:/work" -w /work --tmpfs "/work/node_modules:size=$PNPM_TMPFS_SIZE" || return $?
    install_tmpfs_samples="$MEASURED_SAMPLES"
  fi

  local boot_p50 toolchain_p50 fetch_p50 offline_project_p50 offline_tmpfs_p50 install_project_p50 install_tmpfs_p50
  boot_p50=$(pct "$boot_samples" 50)
  toolchain_p50=$(pct "$toolchain_samples" 50)
  fetch_p50=0
  offline_project_p50=0
  offline_tmpfs_p50=0
  install_project_p50=0
  install_tmpfs_p50=0
  [ -n "$fetch_samples" ] && fetch_p50=$(pct "$fetch_samples" 50)
  [ -n "$offline_project_samples" ] && offline_project_p50=$(pct "$offline_project_samples" 50)
  [ -n "$offline_tmpfs_samples" ] && offline_tmpfs_p50=$(pct "$offline_tmpfs_samples" 50)
  [ -n "$install_project_samples" ] && install_project_p50=$(pct "$install_project_samples" 50)
  [ -n "$install_tmpfs_samples" ] && install_tmpfs_p50=$(pct "$install_tmpfs_samples" 50)

  echo "  boot baseline:          p50=${boot_p50}ms p90=$(pct "$boot_samples" 90)ms"
  echo "  corepack+pnpm baseline: p50=${toolchain_p50}ms p90=$(pct "$toolchain_samples" 90)ms"

  if [ -n "$fetch_samples" ]; then
    echo "  fetch/download+extract: p50=${fetch_p50}ms p90=$(pct "$fetch_samples" 90)ms target=pnpm-store"
    echo "  offline install fs:     p50=${offline_project_p50}ms p90=$(pct "$offline_project_samples" 90)ms target=project-mount"
    if [ -n "$offline_tmpfs_samples" ]; then
      echo "  offline install fs:     p50=${offline_tmpfs_p50}ms p90=$(pct "$offline_tmpfs_samples" 90)ms target=tmpfs"
      echo "  project fs overhead:    p50=$(( offline_project_p50 - offline_tmpfs_p50 ))ms vs tmpfs"
    fi
  else
    echo "  fetch/offline split:    skipped (requires PNPM_CACHE=1 so store survives between boxes)"
  fi

  if [ -n "$install_project_samples" ]; then
    echo "  frozen install total:   p50=${install_project_p50}ms p90=$(pct "$install_project_samples" 90)ms target=project-mount cache=$PNPM_CACHE"
  fi
  if [ -n "$install_tmpfs_samples" ]; then
    echo "  frozen install total:   p50=${install_tmpfs_p50}ms p90=$(pct "$install_tmpfs_samples" 90)ms target=tmpfs cache=$PNPM_CACHE"
  fi

  if [ "$PNPM_DOCKER" = "1" ] && command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
    docker pull "$PNPM_IMAGE" >/dev/null 2>&1 || true
    docker volume rm -f "$PNPM_DOCKER_STORE_VOLUME" >/dev/null 2>&1 || true
    measure_docker_samples "cold-project" "$full_cmd" -v "$project:/work" -w /work || return $?
    local docker_cold_samples="$MEASURED_SAMPLES"
    measure_docker_samples "hot-project" "$full_cmd" -v "$project:/work" -w /work || return $?
    local docker_hot_samples="$MEASURED_SAMPLES"
    local docker_cold_p50 docker_hot_p50
    docker_cold_p50=$(pct "$docker_cold_samples" 50)
    docker_hot_p50=$(pct "$docker_hot_samples" 50)
    echo "  Docker cold baseline:   p50=${docker_cold_p50}ms p90=$(pct "$docker_cold_samples" 90)ms target=project-mount"
    echo "  Docker hot baseline:    p50=${docker_hot_p50}ms p90=$(pct "$docker_hot_samples" 90)ms target=project-mount"
    [ "$install_project_p50" -gt 0 ] && echo "  A3S/Docker hot ratio:   $(ratio "$install_project_p50" "$docker_hot_p50") target=project-mount"
    [ "$install_tmpfs_p50" -gt 0 ] && echo "  A3S tmpfs/Docker hot:   $(ratio "$install_tmpfs_p50" "$docker_hot_p50")"
  elif [ "$PNPM_DOCKER" = "1" ]; then
    echo "  Docker baseline:        skipped (docker CLI or daemon unavailable)"
  fi
}

require_kvm
echo "# a3s-box benchmark — $(uname -sm), image=$IMAGE"
rc=0
case "$MODE" in
  cold) bench_cold ;;
  warm) bench_warm ;;
  fork) bench_fork ;;
  leak) bench_leak || rc=$? ;;
  race) bench_race || rc=$? ;;
  pnpm) bench_pnpm || rc=$? ;;
  all)
    bench_cold
    bench_warm
    bench_fork
    bench_leak || rc=$?
    bench_race || rc=$?
    ;;
  *) echo "unknown mode: $MODE (use all|cold|warm|fork|leak|race|pnpm)"; exit 2 ;;
esac
exit "$rc"
