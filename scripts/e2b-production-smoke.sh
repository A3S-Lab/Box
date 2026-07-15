#!/usr/bin/env bash
# Destructive lifecycle smoke for the ACL-configured production service.
set -euo pipefail

API_KEY="e2b_a1b2c3"
CREDENTIAL_HASH='pbkdf2-sha256$100000$03030303030303030303030303030303$6ea6a4ae29bedfcdff6890292ff1410b45211268631889c254502776af12ff4d'
TOKEN_ENCRYPTION="$(printf '07%.0s' {1..32})"
TOKEN_DIGEST="$(printf '08%.0s' {1..32})"
PORT="${A3S_BOX_E2B_SMOKE_PORT:-38081}"
GATEWAY_PORT="${A3S_BOX_E2B_GATEWAY_SMOKE_PORT:-38443}"
IMAGE="${A3S_BOX_SMOKE_IMAGE:-alpine:3.20}"

fail() {
  printf 'E2B production smoke failed: %s\n' "$*" >&2
  if [[ -n "${LOG:-}" && -f "$LOG" ]]; then
    tail -100 "$LOG" >&2 || true
  fi
  exit 1
}

[[ "${A3S_BOX_E2B_SMOKE:-}" == "1" ]] ||
  fail 'set A3S_BOX_E2B_SMOKE=1 to acknowledge the destructive smoke test'
[[ -n "${A3S_HOME:-}" && -d "$A3S_HOME" ]] ||
  fail 'A3S_HOME must identify a prepared dedicated runtime home'
[[ "$(basename "$A3S_HOME")" == *e2b-service-smoke* ]] ||
  fail 'A3S_HOME must have e2b-service-smoke in its final path component'
[[ -x "${A3S_BOX_E2B_BIN:-}" ]] || fail 'A3S_BOX_E2B_BIN must be executable'
[[ -x "${A3S_BOX_CRUN_PATH:-}" ]] || fail 'A3S_BOX_CRUN_PATH must be executable'
[[ "$(realpath "$A3S_BOX_CRUN_PATH")" == "$(realpath "$A3S_HOME/bin/crun")" ]] ||
  fail 'A3S_BOX_CRUN_PATH must equal A3S_HOME/bin/crun'
[[ -x "$A3S_HOME/bin/a3s-box-guest-init" ]] || fail 'guest init is missing'
[[ -x "$A3S_HOME/bin/a3s-box-shim" ]] || fail 'shim is missing'
[[ "$PORT" =~ ^[0-9]+$ && "$PORT" -gt 0 && "$PORT" -le 65535 ]] ||
  fail 'A3S_BOX_E2B_SMOKE_PORT must be a valid TCP port'
[[ "$GATEWAY_PORT" =~ ^[0-9]+$ && "$GATEWAY_PORT" -gt 0 && "$GATEWAY_PORT" -le 65535 ]] ||
  fail 'A3S_BOX_E2B_GATEWAY_SMOKE_PORT must be a valid TCP port'
[[ "$GATEWAY_PORT" != "$PORT" ]] || fail 'control and gateway ports must differ'
command -v openssl >/dev/null || fail 'openssl is required for the TLS gateway smoke'

umask 077
STATE_DIR="$A3S_HOME/e2b-compat-smoke"
CONFIG="$STATE_DIR/service.acl"
LOG="$STATE_DIR/service.log"
TLS_CERT="$STATE_DIR/gateway-cert.pem"
TLS_KEY="$STATE_DIR/gateway-key.pem"
BASE_URL="http://127.0.0.1:$PORT"
SERVICE_PID=""
SANDBOX_ID=""
ENVD_TOKEN=""
TRAFFIC_TOKEN=""

stop_service() {
  if [[ -n "$SERVICE_PID" ]] && kill -0 "$SERVICE_PID" 2>/dev/null; then
    kill -TERM "$SERVICE_PID"
    wait "$SERVICE_PID" || true
  fi
  SERVICE_PID=""
}

wait_ready() {
  local attempts=0
  while (( attempts < 100 )); do
    if curl --silent --output /dev/null \
      --header "X-API-Key: $API_KEY" "$BASE_URL/v2/sandboxes"; then
      return 0
    fi
    if [[ -n "$SERVICE_PID" ]] && ! kill -0 "$SERVICE_PID" 2>/dev/null; then
      printf '%s\n' 'service exited before becoming ready' >&2
      tail -100 "$LOG" >&2 || true
      return 1
    fi
    attempts=$((attempts + 1))
    sleep 0.1
  done
  printf '%s\n' 'service readiness timed out' >&2
  tail -100 "$LOG" >&2 || true
  return 1
}

start_service() {
  : >"$LOG"
  env \
    A3S_HOME="$A3S_HOME" \
    A3S_BOX_CRUN_PATH="$A3S_BOX_CRUN_PATH" \
    TOKEN_ENCRYPTION="$TOKEN_ENCRYPTION" \
    TOKEN_DIGEST="$TOKEN_DIGEST" \
    RUST_LOG="${RUST_LOG:-a3s_box_compat=info}" \
    "$A3S_BOX_E2B_BIN" --config "$CONFIG" >"$LOG" 2>&1 &
  SERVICE_PID=$!
  wait_ready
}

status_request() {
  local method="$1"
  local path="$2"
  local output="$3"
  local body="${4:-}"
  local arguments=(
    --silent --show-error --output "$output" --write-out '%{http_code}'
    --request "$method" --header "X-API-Key: $API_KEY"
  )
  if [[ -n "$body" ]]; then
    arguments+=(--header 'Content-Type: application/json' --data "$body")
  fi
  curl "${arguments[@]}" "$BASE_URL$path"
}

gateway_status() {
  local host="$1"
  local output="$2"
  local token_header="$3"
  local token="$4"
  shift 4
  curl --silent --show-error --output "$output" --write-out '%{http_code}' \
    --cacert "$TLS_CERT" \
    --resolve "$host:$GATEWAY_PORT:127.0.0.1" \
    --header "$token_header: $token" \
    "$@" "https://$host:$GATEWAY_PORT/health"
}

wait_gateway_ready() {
  local host="$1"
  local attempts=0
  local status=""
  while (( attempts < 100 )); do
    status="$(gateway_status "$host" "$STATE_DIR/gateway-body.txt" X-Access-Token "$ENVD_TOKEN" || true)"
    if [[ "$status" == "200" ]]; then
      return 0
    fi
    if [[ -n "$SERVICE_PID" ]] && ! kill -0 "$SERVICE_PID" 2>/dev/null; then
      tail -100 "$LOG" >&2 || true
      return 1
    fi
    attempts=$((attempts + 1))
    sleep 0.1
  done
  printf 'gateway readiness timed out with HTTP %s\n' "$status" >&2
  tail -100 "$LOG" >&2 || true
  return 1
}

json_field() {
  python3 - "$1" "$2" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    value = json.load(source)
for component in sys.argv[2].split("."):
    value = value[component]
print(value)
PY
}

preserve_failure_diagnostics() {
  local diagnostics="$STATE_DIR/failure-diagnostics"
  local records="$STATE_DIR/managed-executions.json"
  mkdir -p "$diagnostics"
  [[ -f "$records" ]] || return 0

  while IFS=$'\t' read -r execution_id pid pid_start_time; do
    [[ -n "$execution_id" && "$execution_id" != *[!a-zA-Z0-9._-]* ]] || continue
    local execution_diagnostics="$diagnostics/$execution_id"
    local box_dir="$A3S_HOME/boxes/$execution_id"
    local runtime_root="$A3S_HOME/run/crun/$execution_id"
    mkdir -p "$execution_diagnostics"
    printf 'pid=%s\npid_start_time=%s\n' "$pid" "$pid_start_time" \
      >"$execution_diagnostics/process-identity.txt"
    if [[ "$pid" =~ ^[0-9]+$ && -r "/proc/$pid/stat" ]]; then
      cp "/proc/$pid/stat" "$execution_diagnostics/proc-stat.txt" || true
      readlink "/proc/$pid/ns/net" \
        >"$execution_diagnostics/network-namespace.txt" 2>&1 || true
    fi
    if [[ -d "$box_dir/logs" ]]; then
      cp -a "$box_dir/logs" "$execution_diagnostics/logs" || true
    fi
    for relative_path in \
      sandbox/runtime.json \
      sandbox/bundle/config.json \
      sandbox/bundle/execution-plan.json \
      sandbox/bundle/capabilities.json; do
      if [[ -f "$box_dir/$relative_path" ]]; then
        mkdir -p "$execution_diagnostics/$(dirname "$relative_path")"
        cp "$box_dir/$relative_path" \
          "$execution_diagnostics/$relative_path" || true
      fi
    done
    "$A3S_BOX_CRUN_PATH" --root "$runtime_root" state "$execution_id" \
      >"$execution_diagnostics/crun-state.json" \
      2>"$execution_diagnostics/crun-state.stderr" || true
  done < <(python3 - "$records" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    records = json.load(source)
for record in records:
    print(
        record.get("id", ""),
        record.get("pid") or "",
        record.get("pid_start_time") or "",
        sep="\t",
    )
PY
)
}

cleanup() {
  local exit_code=$?
  trap - EXIT INT TERM
  set +e
  if [[ "$exit_code" -ne 0 && "${A3S_BOX_E2B_KEEP_STATE_ON_FAILURE:-}" == "1" ]]; then
    preserve_failure_diagnostics
  fi
  if [[ -n "$SANDBOX_ID" ]]; then
    if [[ -z "$SERVICE_PID" ]] || ! kill -0 "$SERVICE_PID" 2>/dev/null; then
      start_service >/dev/null 2>&1
    fi
    status_request DELETE "/sandboxes/$SANDBOX_ID" /dev/null >/dev/null 2>&1
  fi
  stop_service
  if [[ "$exit_code" -ne 0 && "${A3S_BOX_E2B_KEEP_STATE_ON_FAILURE:-}" == "1" ]]; then
    printf 'Preserved failed smoke state at %s\n' "$STATE_DIR" >&2
  else
    rm -rf "$STATE_DIR"
  fi
  exit "$exit_code"
}
trap cleanup EXIT INT TERM

rm -rf "$STATE_DIR"
mkdir -p "$STATE_DIR"
openssl req -x509 -newkey rsa:2048 -sha256 -nodes -days 1 \
  -subj '/CN=*.box.example.com' \
  -addext 'subjectAltName=DNS:*.box.example.com,DNS:sandbox.box.example.com' \
  -keyout "$TLS_KEY" -out "$TLS_CERT" >/dev/null 2>&1
cat >"$CONFIG" <<EOF
e2b_compat {
  api_listen = "127.0.0.1:$PORT"
  api_public_url = "$BASE_URL"
  sandbox_domain = "box.example.com"
  database_path = "$STATE_DIR/lifecycle.sqlite3"
  runtime_home = "$A3S_HOME"
  runtime_state_path = "$STATE_DIR/managed-executions.json"

  gateway {
    listen = "127.0.0.1:$GATEWAY_PORT"
    tls_certificate_path = "$TLS_CERT"
    tls_private_key_path = "$TLS_KEY"
    max_connections = 128
    handshake_timeout_ms = 5000
    connect_timeout_ms = 2000
    drain_timeout_seconds = 5
  }

  supervisor {
    interval_seconds = 1
    batch_size = 10
    reconciliation_page_size = 10
  }

  account "smoke" {
    scheme = "api_key"
    owner_id = "smoke-owner"
    client_id = "smoke-client"
    hash = "$CREDENTIAL_HASH"
  }

  token_key "smoke" {
    version = 1
    active = true
    encryption_key = env("TOKEN_ENCRYPTION")
    digest_key = env("TOKEN_DIGEST")
  }

  template_policy "fixture-template" {
    image = "$IMAGE"
    envd_version = "0.1.3"
    isolation = "sandbox"
    network = "none"
    command = ["/bin/sh", "-c", "mkdir -p /tmp/e2b-smoke && echo sandbox-data-plane > /tmp/e2b-smoke/health && exec busybox httpd -f -p 49983 -h /tmp/e2b-smoke"]

    resources {
      vcpus = 2
      memory_mb = 512
      disk_mb = 1024
    }

    route {
      port = 49999
      token_scope = "traffic"
    }
  }
}
EOF

start_service

CREATE_RESPONSE="$STATE_DIR/create.json"
CREATE_STATUS="$(status_request POST /sandboxes "$CREATE_RESPONSE" \
  '{"templateID":"fixture-template","timeout":60,"metadata":{"test":"production-service"},"envVars":{"SMOKE":"true"},"secure":true,"allow_internet_access":false}')"
[[ "$CREATE_STATUS" == "201" ]] || fail "create returned HTTP $CREATE_STATUS"
SANDBOX_ID="$(json_field "$CREATE_RESPONSE" sandboxID)"
[[ "$SANDBOX_ID" == sandbox-* ]] || fail 'create returned an invalid sandbox ID'
[[ "$(json_field "$CREATE_RESPONSE" domain)" == "box.example.com" ]] ||
  fail 'create returned the wrong sandbox domain'
ENVD_TOKEN="$(json_field "$CREATE_RESPONSE" envdAccessToken)"
TRAFFIC_TOKEN="$(json_field "$CREATE_RESPONSE" trafficAccessToken)"
[[ -n "$ENVD_TOKEN" ]] ||
  fail 'create omitted the envd access token'
[[ -n "$TRAFFIC_TOKEN" ]] ||
  fail 'create omitted the traffic access token'

DIRECT_HOST="49983-$SANDBOX_ID.box.example.com"
wait_gateway_ready "$DIRECT_HOST" || fail 'TLS direct route did not become ready'
[[ "$(cat "$STATE_DIR/gateway-body.txt")" == "sandbox-data-plane" ]] ||
  fail 'TLS gateway returned the wrong Sandbox response body'
[[ "$(gateway_status "$DIRECT_HOST" /dev/null X-Access-Token wrong-token)" == "401" ]] ||
  fail 'TLS gateway accepted an invalid envd token'
[[ "$(gateway_status "$DIRECT_HOST" /dev/null E2B-Traffic-Access-Token "$TRAFFIC_TOKEN")" == "401" ]] ||
  fail 'TLS gateway accepted a traffic token for the envd scope'
[[ "$(gateway_status sandbox.box.example.com "$STATE_DIR/shared-body.txt" X-Access-Token "$ENVD_TOKEN" \
  --header "E2b-Sandbox-Id: $SANDBOX_ID" --header 'E2b-Sandbox-Port: 49983')" == "200" ]] ||
  fail 'TLS shared route did not return HTTP 200'
[[ "$(cat "$STATE_DIR/shared-body.txt")" == "sandbox-data-plane" ]] ||
  fail 'TLS shared route returned the wrong Sandbox response body'

DETAIL_RESPONSE="$STATE_DIR/detail.json"
[[ "$(status_request GET "/sandboxes/$SANDBOX_ID" "$DETAIL_RESPONSE")" == "200" ]] ||
  fail 'get did not return HTTP 200'
[[ "$(json_field "$DETAIL_RESPONSE" state)" == "running" ]] ||
  fail 'new sandbox is not running'

stop_service
start_service

[[ "$(status_request GET "/sandboxes/$SANDBOX_ID" "$DETAIL_RESPONSE")" == "200" ]] ||
  fail 'sandbox was unavailable after service restart'
[[ "$(json_field "$DETAIL_RESPONSE" state)" == "running" ]] ||
  fail 'startup reconciliation did not preserve the running sandbox'
wait_gateway_ready "$DIRECT_HOST" || fail 'TLS route was unavailable after service restart'

CONNECT_RESPONSE="$STATE_DIR/connect.json"
[[ "$(status_request POST "/sandboxes/$SANDBOX_ID/connect" "$CONNECT_RESPONSE" '{"timeout":45}')" == "200" ]] ||
  fail 'connect did not return HTTP 200'
[[ "$(json_field "$CONNECT_RESPONSE" sandboxID)" == "$SANDBOX_ID" ]] ||
  fail 'connect returned a different sandbox ID'

[[ "$(status_request POST "/sandboxes/$SANDBOX_ID/timeout" /dev/null '{"timeout":30}')" == "204" ]] ||
  fail 'timeout replacement did not return HTTP 204'
[[ "$(status_request DELETE "/sandboxes/$SANDBOX_ID" /dev/null)" == "204" ]] ||
  fail 'kill did not return HTTP 204'
[[ "$(gateway_status "$DIRECT_HOST" /dev/null X-Access-Token "$ENVD_TOKEN")" == "404" ]] ||
  fail 'stale TLS route remained available after kill'

EXECUTION_ID="$(python3 - "$STATE_DIR/managed-executions.json" "$SANDBOX_ID" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    records = json.load(source)
matches = [
    record for record in records
    if record.get("managed_execution", {}).get("request", {}).get("external_sandbox_id") == sys.argv[2]
]
if len(matches) != 1:
    raise SystemExit("managed execution record is missing or ambiguous")
if matches[0].get("status") != "stopped":
    raise SystemExit("managed execution did not persist stopped state")
print(matches[0]["id"])
PY
)"
[[ ! -e "$A3S_HOME/boxes/$EXECUTION_ID" ]] || fail 'box directory leaked after kill'
[[ ! -e "$A3S_HOME/run/crun/$EXECUTION_ID" ]] || fail 'crun state leaked after kill'
[[ ! -e "/tmp/a3s-box-sockets/$EXECUTION_ID" ]] || fail 'runtime socket directory leaked after kill'

SANDBOX_ID=""
stop_service
printf 'E2B production smoke passed: lifecycle, TLS data plane, restart recovery, credentials, and cleanup\n'
