#!/usr/bin/env bash
#
# crictl-smoke-test.sh — Validate A3S Box CRI implementation using crictl.
#
# Prerequisites:
#   - crictl installed (https://github.com/kubernetes-sigs/cri-tools)
#   - a3s-box-cri running and serving on the configured socket
#
# Usage:
#   export CONTAINER_RUNTIME_ENDPOINT=unix:///var/run/a3s-box/a3s-box.sock
#   bash deploy/scripts/crictl-smoke-test.sh
#
# Exit codes:
#   0 — All tests passed
#   1 — One or more tests failed

set -euo pipefail

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

PASS=0
FAIL=0
SANDBOX_ID=""
CONTAINER_ID=""

# Default socket if not set
: "${CONTAINER_RUNTIME_ENDPOINT:=unix:///var/run/a3s-box/a3s-box.sock}"
export CONTAINER_RUNTIME_ENDPOINT

log_pass() {
    echo -e "${GREEN}✓ PASS${NC}: $1"
    PASS=$((PASS + 1))
}

log_fail() {
    echo -e "${RED}✗ FAIL${NC}: $1"
    FAIL=$((FAIL + 1))
}

log_info() {
    echo -e "${YELLOW}→${NC} $1"
}

cleanup() {
    log_info "Cleaning up..."
    if [ -n "$CONTAINER_ID" ]; then
        crictl stop "$CONTAINER_ID" 2>/dev/null || true
        crictl rm "$CONTAINER_ID" 2>/dev/null || true
    fi
    if [ -n "$SANDBOX_ID" ]; then
        crictl stopp "$SANDBOX_ID" 2>/dev/null || true
        crictl rmp "$SANDBOX_ID" 2>/dev/null || true
    fi
}

trap cleanup EXIT

echo "============================================"
echo "  A3S Box CRI Smoke Test"
echo "  Socket: $CONTAINER_RUNTIME_ENDPOINT"
echo "============================================"
echo ""

# --- Test 1: Version ---
log_info "Test 1: crictl version"
if crictl version 2>&1 | grep -q "RuntimeName.*a3s-box"; then
    log_pass "Runtime version returned (a3s-box)"
else
    log_fail "Runtime version check failed"
fi

# --- Test 2: Runtime status ---
log_info "Test 2: crictl info"
if crictl info 2>&1 | grep -q "RuntimeReady"; then
    log_pass "Runtime status is ready"
else
    log_fail "Runtime status check failed"
fi

# --- Test 3: Pull image ---
log_info "Test 3: crictl pull alpine:latest"
if crictl pull alpine:latest 2>&1; then
    log_pass "Image pulled successfully"
else
    log_fail "Image pull failed"
fi

# --- Test 4: List images ---
log_info "Test 4: crictl images"
if crictl images 2>&1 | grep -q "alpine"; then
    log_pass "Image listed after pull"
else
    log_fail "Image not found in list"
fi

# --- Test 5: Image status ---
log_info "Test 5: crictl inspecti alpine:latest"
if crictl inspecti alpine:latest 2>&1 | grep -q "alpine"; then
    log_pass "Image status returned"
else
    log_fail "Image status check failed"
fi

# --- Test 6: Run pod sandbox ---
log_info "Test 6: crictl runp (create pod sandbox)"
SANDBOX_CONFIG=$(mktemp)
cat > "$SANDBOX_CONFIG" <<EOF
{
  "metadata": {
    "name": "smoke-test-pod",
    "namespace": "default",
    "uid": "smoke-test-uid-001"
  },
  "log_directory": "/tmp/a3s-box-smoke-test"
}
EOF

if SANDBOX_ID=$(crictl runp "$SANDBOX_CONFIG" 2>&1); then
    log_pass "Pod sandbox created: ${SANDBOX_ID:0:12}"
else
    log_fail "Pod sandbox creation failed"
    SANDBOX_ID=""
fi
rm -f "$SANDBOX_CONFIG"

# --- Test 7: Pod sandbox status ---
if [ -n "$SANDBOX_ID" ]; then
    log_info "Test 7: crictl inspectp $SANDBOX_ID"
    if crictl inspectp "$SANDBOX_ID" 2>&1 | grep -q "SANDBOX_READY"; then
        log_pass "Pod sandbox is ready"
    else
        log_fail "Pod sandbox not ready"
    fi
else
    log_fail "Skipped: no sandbox ID"
fi

# --- Test 8: List pod sandboxes ---
log_info "Test 8: crictl pods"
if crictl pods 2>&1 | grep -q "smoke-test-pod"; then
    log_pass "Pod sandbox listed"
else
    log_fail "Pod sandbox not found in list"
fi

# --- Test 9: Create container ---
if [ -n "$SANDBOX_ID" ]; then
    log_info "Test 9: crictl create (container in sandbox)"
    CONTAINER_CONFIG=$(mktemp)
    cat > "$CONTAINER_CONFIG" <<EOF
{
  "metadata": {
    "name": "smoke-test-container"
  },
  "image": {
    "image": "alpine:latest"
  },
  "command": ["sleep", "30"],
  "log_path": "smoke-test-container.log"
}
EOF
    SANDBOX_CONFIG2=$(mktemp)
    cat > "$SANDBOX_CONFIG2" <<EOF
{
  "metadata": {
    "name": "smoke-test-pod",
    "namespace": "default",
    "uid": "smoke-test-uid-001"
  }
}
EOF

    if CONTAINER_ID=$(crictl create "$SANDBOX_ID" "$CONTAINER_CONFIG" "$SANDBOX_CONFIG2" 2>&1); then
        log_pass "Container created: ${CONTAINER_ID:0:12}"
    else
        log_fail "Container creation failed"
        CONTAINER_ID=""
    fi
    rm -f "$CONTAINER_CONFIG" "$SANDBOX_CONFIG2"
else
    log_fail "Skipped: no sandbox ID"
fi

# --- Test 10: Start container ---
if [ -n "$CONTAINER_ID" ]; then
    log_info "Test 10: crictl start $CONTAINER_ID"
    if crictl start "$CONTAINER_ID" 2>&1; then
        log_pass "Container started"
    else
        log_fail "Container start failed"
    fi
else
    log_fail "Skipped: no container ID"
fi

# --- Test 11: Container status ---
if [ -n "$CONTAINER_ID" ]; then
    log_info "Test 11: crictl inspect $CONTAINER_ID"
    if crictl inspect "$CONTAINER_ID" 2>&1 | grep -q "CONTAINER_RUNNING"; then
        log_pass "Container is running"
    else
        log_fail "Container not running"
    fi
else
    log_fail "Skipped: no container ID"
fi

# --- Test 12: Exec in container ---
if [ -n "$CONTAINER_ID" ]; then
    log_info "Test 12: crictl exec (echo hello)"
    if OUTPUT=$(crictl exec "$CONTAINER_ID" echo hello 2>&1) && echo "$OUTPUT" | grep -q "hello"; then
        log_pass "Exec returned expected output"
    else
        log_fail "Exec failed or unexpected output"
    fi
else
    log_fail "Skipped: no container ID"
fi

# --- Test 13: List containers ---
log_info "Test 13: crictl ps"
if crictl ps 2>&1 | grep -q "smoke-test-container"; then
    log_pass "Container listed"
else
    log_fail "Container not found in list"
fi

# --- Test 14: Stop container ---
if [ -n "$CONTAINER_ID" ]; then
    log_info "Test 14: crictl stop $CONTAINER_ID"
    if crictl stop "$CONTAINER_ID" 2>&1; then
        log_pass "Container stopped"
    else
        log_fail "Container stop failed"
    fi
else
    log_fail "Skipped: no container ID"
fi

# --- Test 15: Remove container ---
if [ -n "$CONTAINER_ID" ]; then
    log_info "Test 15: crictl rm $CONTAINER_ID"
    if crictl rm "$CONTAINER_ID" 2>&1; then
        log_pass "Container removed"
        CONTAINER_ID=""  # Prevent double-cleanup
    else
        log_fail "Container remove failed"
    fi
else
    log_fail "Skipped: no container ID"
fi

# --- Test 16: Stop pod sandbox ---
if [ -n "$SANDBOX_ID" ]; then
    log_info "Test 16: crictl stopp $SANDBOX_ID"
    if crictl stopp "$SANDBOX_ID" 2>&1; then
        log_pass "Pod sandbox stopped"
    else
        log_fail "Pod sandbox stop failed"
    fi
else
    log_fail "Skipped: no sandbox ID"
fi

# --- Test 17: Remove pod sandbox ---
if [ -n "$SANDBOX_ID" ]; then
    log_info "Test 17: crictl rmp $SANDBOX_ID"
    if crictl rmp "$SANDBOX_ID" 2>&1; then
        log_pass "Pod sandbox removed"
        SANDBOX_ID=""  # Prevent double-cleanup
    else
        log_fail "Pod sandbox remove failed"
    fi
else
    log_fail "Skipped: no sandbox ID"
fi

# --- Test 18: Remove image ---
log_info "Test 18: crictl rmi alpine:latest"
if crictl rmi alpine:latest 2>&1; then
    log_pass "Image removed"
else
    log_fail "Image remove failed"
fi

# --- Summary ---
echo ""
echo "============================================"
echo "  Results: ${GREEN}${PASS} passed${NC}, ${RED}${FAIL} failed${NC}"
echo "============================================"

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
exit 0
