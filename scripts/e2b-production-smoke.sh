#!/usr/bin/env bash
# Destructive lifecycle smoke for the ACL-configured production service.
set -euo pipefail

API_KEY="e2b_a1b2c3"
CREDENTIAL_HASH='pbkdf2-sha256$100000$03030303030303030303030303030303$6ea6a4ae29bedfcdff6890292ff1410b45211268631889c254502776af12ff4d'
TOKEN_ENCRYPTION="$(printf '07%.0s' {1..32})"
TOKEN_DIGEST="$(printf '08%.0s' {1..32})"
PORT="${A3S_BOX_E2B_SMOKE_PORT:-38081}"
GATEWAY_PORT="${A3S_BOX_E2B_GATEWAY_SMOKE_PORT:-38443}"
GATEWAY_ADDRESS="${A3S_BOX_E2B_GATEWAY_SMOKE_ADDRESS:-127.0.0.1}"
SANDBOX_DOMAIN="${A3S_BOX_E2B_SANDBOX_DOMAIN:-localhost.localdomain}"
SANDBOX_PUBLIC_DOMAIN="$SANDBOX_DOMAIN"
if [[ "$GATEWAY_PORT" != "443" ]]; then
  SANDBOX_PUBLIC_DOMAIN="$SANDBOX_DOMAIN:$GATEWAY_PORT"
fi
IMAGE="${A3S_BOX_SMOKE_IMAGE:-alpine:3.20}"
RUNTIME_IMAGE="${A3S_BOX_E2B_RUNTIME_IMAGE:-}"
EXPECTED_TRAFFIC_BODY="sandbox-data-plane"
if [[ -n "$RUNTIME_IMAGE" ]]; then
  EXPECTED_TRAFFIC_BODY='"OK"'
fi
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
OFFICIAL_CLIENT_RUNNER="${A3S_BOX_E2B_OFFICIAL_CLIENT_RUNNER:-$SCRIPT_DIR/../compat/e2b/fixtures/official-clients/run_production.py}"
# A dependency-free HTTP/1.1 responder used with BusyBox nc -e. Alpine's
# BusyBox build does not guarantee the optional httpd applet.
HTTP_RESPONDER_B64='IyEvYmluL3NoCndoaWxlIElGUz0gcmVhZCAtciBsaW5lOyBkbwogIFsgIiRsaW5lIiA9ICIkKHByaW50ZiAnXHInKSIgXSAmJiBicmVhawpkb25lCmJvZHk9J3NhbmRib3gtZGF0YS1wbGFuZScKcHJpbnRmICdIVFRQLzEuMSAyMDAgT0tcclxuQ29udGVudC1MZW5ndGg6ICVzXHJcbkNvbm5lY3Rpb246IGNsb3NlXHJcbkNvbnRlbnQtVHlwZTogdGV4dC9wbGFpblxyXG5cclxuJXMnICIkeyNib2R5fSIgIiRib2R5Igo='
BASH_COMPAT_WRAPPER_B64='IyEvYmluL3NoCmV4ZWMgL2Jpbi9zaCAiJEAiCg=='

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
if [[ "$GATEWAY_ADDRESS" == *:* ]]; then
  GATEWAY_LISTEN="[$GATEWAY_ADDRESS]:$GATEWAY_PORT"
  GATEWAY_RESOLVE="[$GATEWAY_ADDRESS]"
else
  GATEWAY_LISTEN="$GATEWAY_ADDRESS:$GATEWAY_PORT"
  GATEWAY_RESOLVE="$GATEWAY_ADDRESS"
fi
if ! python3 - "$GATEWAY_ADDRESS" <<'PY'
import ipaddress
import sys

address = ipaddress.ip_address(sys.argv[1])
if not address.is_loopback:
    raise SystemExit(1)
PY
then
  fail 'A3S_BOX_E2B_GATEWAY_SMOKE_ADDRESS must be a loopback IP address'
fi
[[ "$SANDBOX_DOMAIN" =~ ^[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?$ ]] ||
  fail 'A3S_BOX_E2B_SANDBOX_DOMAIN must be a DNS name'
DNS_PREFLIGHT_HOST="a3s-e2b-preflight.$SANDBOX_DOMAIN"
if ! python3 - "$DNS_PREFLIGHT_HOST" "$GATEWAY_ADDRESS" <<'PY'
import ipaddress
import socket
import sys

hostname = sys.argv[1]
expected = ipaddress.ip_address(sys.argv[2])
try:
    addresses = {
        ipaddress.ip_address(item[4][0])
        for item in socket.getaddrinfo(hostname, None)
    }
except OSError as error:
    raise SystemExit(f"{hostname} did not resolve: {error}") from error
if expected not in addresses:
    rendered = ", ".join(sorted(str(address) for address in addresses))
    raise SystemExit(
        f"{hostname} resolved to [{rendered}], not gateway {expected}"
    )
PY
then
  fail 'Sandbox wildcard DNS does not resolve to the configured loopback gateway'
fi
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

gateway_request() {
  local host="$1"
  local output="$2"
  local token_header="$3"
  local token="$4"
  local path="$5"
  shift 5
  curl --silent --show-error --output "$output" --write-out '%{http_code}' \
    --noproxy '*' \
    --cacert "$TLS_CERT" \
    --resolve "$host:$GATEWAY_PORT:$GATEWAY_RESOLVE" \
    --header "$token_header: $token" \
    "$@" "https://$host:$GATEWAY_PORT$path"
}

gateway_status() {
  local host="$1"
  local output="$2"
  local token_header="$3"
  local token="$4"
  shift 4
  gateway_request "$host" "$output" "$token_header" "$token" /health "$@"
}

wait_gateway_ready() {
  local host="$1"
  local attempts=0
  local status=""
  while (( attempts < 100 )); do
    status="$(gateway_status "$host" "$STATE_DIR/gateway-body.txt" X-Access-Token "$ENVD_TOKEN" || true)"
    if [[ "$status" == "204" ]]; then
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
    # crun materializes a missing --root directory even for an absent
    # container. Keep failure diagnostics side-effect free when startup did
    # not reach the runtime.
    if [[ -e "$runtime_root" ]]; then
      "$A3S_BOX_CRUN_PATH" --root "$runtime_root" state "$execution_id" \
        >"$execution_diagnostics/crun-state.json" \
        2>"$execution_diagnostics/crun-state.stderr" || true
    else
      printf 'runtime root was absent; crun state probe skipped\n' \
        >"$execution_diagnostics/crun-state.stderr"
    fi
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
  -subj "/CN=*.$SANDBOX_DOMAIN" \
  -addext "subjectAltName=DNS:*.$SANDBOX_DOMAIN,DNS:sandbox.$SANDBOX_DOMAIN" \
  -keyout "$TLS_KEY" -out "$TLS_CERT" >/dev/null 2>&1
openssl verify -CAfile "$TLS_CERT" -verify_hostname "$DNS_PREFLIGHT_HOST" \
  "$TLS_CERT" >/dev/null ||
  fail 'generated wildcard TLS certificate does not cover Sandbox routes'
cat >"$CONFIG" <<EOF
e2b_compat {
  api_listen = "127.0.0.1:$PORT"
  api_public_url = "$BASE_URL"
  sandbox_domain = "$SANDBOX_DOMAIN"
  sandbox_public_domain = "$SANDBOX_PUBLIC_DOMAIN"
  database_path = "$STATE_DIR/lifecycle.sqlite3"
  runtime_home = "$A3S_HOME"
  runtime_state_path = "$STATE_DIR/managed-executions.json"

  gateway {
    listen = "$GATEWAY_LISTEN"
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
EOF

append_template_policy() {
  local template_id="$1"
  local template_image="$IMAGE"
  local envd_version="0.1.3"
  if [[ -n "$RUNTIME_IMAGE" ]]; then
    template_image="$RUNTIME_IMAGE"
    envd_version="0.6.9"
  fi

  cat >>"$CONFIG" <<EOF

  template_policy "$template_id" {
    image = "$template_image"
    envd_version = "$envd_version"
    isolation = "sandbox"
    network = "none"
EOF

  if [[ -n "$RUNTIME_IMAGE" ]]; then
    cat >>"$CONFIG" <<EOF
    envd_mode = "runtime"

    resources {
      vcpus = 2
      memory_mb = 2048
      disk_mb = 8192
    }

    route {
      port = 49983
      token_scope = "envd"
    }

    route {
      port = 49999
      token_scope = "traffic"
    }
EOF
  else
    cat >>"$CONFIG" <<EOF
    command = ["/bin/sh", "-c", "adduser -D -u 1000 user >/dev/null 2>&1 || true; printf '%s' '$BASH_COMPAT_WRAPPER_B64' | /bin/busybox base64 -d > /bin/bash && chmod 755 /bin/bash && mkdir -p /tmp/e2b-smoke && printf '%s' '$HTTP_RESPONDER_B64' | /bin/busybox base64 -d > /tmp/e2b-smoke/respond && chmod 755 /tmp/e2b-smoke/respond && exec /bin/busybox nc -lk -p 49999 -e /tmp/e2b-smoke/respond"]

    resources {
      vcpus = 2
      memory_mb = 512
      disk_mb = 1024
    }

    route {
      port = 49999
      token_scope = "traffic"
    }
EOF
  fi

  printf '  }\n' >>"$CONFIG"
}

append_template_policy fixture-template
append_template_policy code-interpreter-v1
printf '}\n' >>"$CONFIG"

start_service

CREATE_RESPONSE="$STATE_DIR/create.json"
CREATE_STATUS="$(status_request POST /sandboxes "$CREATE_RESPONSE" \
  '{"templateID":"fixture-template","timeout":60,"metadata":{"test":"production-service"},"envVars":{"SMOKE":"true"},"secure":true,"allow_internet_access":false}')"
[[ "$CREATE_STATUS" == "201" ]] || fail "create returned HTTP $CREATE_STATUS"
SANDBOX_ID="$(json_field "$CREATE_RESPONSE" sandboxID)"
[[ "$SANDBOX_ID" == sandbox-* ]] || fail 'create returned an invalid sandbox ID'
[[ "$(json_field "$CREATE_RESPONSE" domain)" == "$SANDBOX_PUBLIC_DOMAIN" ]] ||
  fail 'create returned the wrong sandbox domain'
ENVD_TOKEN="$(json_field "$CREATE_RESPONSE" envdAccessToken)"
TRAFFIC_TOKEN="$(json_field "$CREATE_RESPONSE" trafficAccessToken)"
[[ -n "$ENVD_TOKEN" ]] ||
  fail 'create omitted the envd access token'
[[ -n "$TRAFFIC_TOKEN" ]] ||
  fail 'create omitted the traffic access token'

V1_LIST_RESPONSE="$STATE_DIR/list-v1.json"
[[ "$(status_request GET '/sandboxes?metadata=test%3Dproduction-service' "$V1_LIST_RESPONSE")" == "200" ]] ||
  fail 'v1 sandbox list did not return HTTP 200'
if ! python3 - "$V1_LIST_RESPONSE" "$SANDBOX_ID" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    sandboxes = json.load(source)
if not any(sandbox.get("sandboxID") == sys.argv[2] for sandbox in sandboxes):
    raise SystemExit("created Sandbox was absent")
PY
then
  fail 'v1 sandbox list omitted the created Sandbox'
fi

REFRESH_BEFORE_RESPONSE="$STATE_DIR/refresh-before.json"
[[ "$(status_request GET "/sandboxes/$SANDBOX_ID" "$REFRESH_BEFORE_RESPONSE")" == "200" ]] ||
  fail 'sandbox detail before refresh did not return HTTP 200'
REFRESH_BEFORE_END_AT="$(json_field "$REFRESH_BEFORE_RESPONSE" endAt)"
[[ "$(status_request POST "/sandboxes/$SANDBOX_ID/refreshes" /dev/null '{"duration":55}')" == "204" ]] ||
  fail 'sandbox refresh did not return HTTP 204'
[[ "$(status_request POST "/sandboxes/$SANDBOX_ID/refreshes" /dev/null '{}')" == "204" ]] ||
  fail 'sandbox refresh with an empty object did not return HTTP 204'
[[ "$(status_request POST "/sandboxes/$SANDBOX_ID/refreshes" /dev/null)" == "204" ]] ||
  fail 'sandbox refresh without a request body did not return HTTP 204'
REFRESH_UNCHANGED_RESPONSE="$STATE_DIR/refresh-unchanged.json"
[[ "$(status_request GET "/sandboxes/$SANDBOX_ID" "$REFRESH_UNCHANGED_RESPONSE")" == "200" ]] ||
  fail 'sandbox detail after short refresh did not return HTTP 200'
[[ "$(json_field "$REFRESH_UNCHANGED_RESPONSE" endAt)" == "$REFRESH_BEFORE_END_AT" ]] ||
  fail 'sandbox refresh shortened the existing timeout'
[[ "$(status_request POST "/sandboxes/$SANDBOX_ID/refreshes" /dev/null '{"duration":3600}')" == "204" ]] ||
  fail 'sandbox refresh extension did not return HTTP 204'
REFRESH_EXTENDED_RESPONSE="$STATE_DIR/refresh-extended.json"
[[ "$(status_request GET "/sandboxes/$SANDBOX_ID" "$REFRESH_EXTENDED_RESPONSE")" == "200" ]] ||
  fail 'sandbox detail after extended refresh did not return HTTP 200'
if ! python3 - "$REFRESH_BEFORE_RESPONSE" "$REFRESH_EXTENDED_RESPONSE" <<'PY'
import datetime
import json
import sys


def end_at(path: str) -> datetime.datetime:
    with open(path, encoding="utf-8") as source:
        value = json.load(source)["endAt"]
    return datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))


if end_at(sys.argv[2]) <= end_at(sys.argv[1]):
    raise SystemExit("refresh did not extend the sandbox timeout")
PY
then
  fail 'sandbox refresh did not extend the existing timeout'
fi

DIRECT_HOST="49983-$SANDBOX_ID.$SANDBOX_DOMAIN"
wait_gateway_ready "$DIRECT_HOST" || fail 'TLS direct route did not become ready'
[[ ! -s "$STATE_DIR/gateway-body.txt" ]] ||
  fail 'envd health returned an unexpected response body'
[[ "$(gateway_status "$DIRECT_HOST" /dev/null X-Access-Token wrong-token)" == "401" ]] ||
  fail 'TLS gateway accepted an invalid envd token'
[[ "$(gateway_status "$DIRECT_HOST" /dev/null E2B-Traffic-Access-Token "$TRAFFIC_TOKEN")" == "401" ]] ||
  fail 'TLS gateway accepted a traffic token for the envd scope'
[[ "$(gateway_status "sandbox.$SANDBOX_DOMAIN" "$STATE_DIR/shared-body.txt" X-Access-Token "$ENVD_TOKEN" \
  --header "E2b-Sandbox-Id: $SANDBOX_ID" --header 'E2b-Sandbox-Port: 49983')" == "204" ]] ||
  fail 'TLS shared envd route did not return HTTP 204'
[[ ! -s "$STATE_DIR/shared-body.txt" ]] ||
  fail 'TLS shared envd health returned an unexpected response body'

CONTROL_LOGS_V1="$STATE_DIR/control-logs-v1.json"
CONTROL_LOGS_V2="$STATE_DIR/control-logs-v2.json"
[[ "$(status_request GET "/sandboxes/$SANDBOX_ID/logs?start=0&limit=1000" "$CONTROL_LOGS_V1")" == "200" ]] ||
  fail 'control-plane v1 logs did not return HTTP 200'
[[ "$(status_request GET "/v2/sandboxes/$SANDBOX_ID/logs?direction=backward&limit=1000" "$CONTROL_LOGS_V2")" == "200" ]] ||
  fail 'control-plane v2 logs did not return HTTP 200'
if ! python3 - "$CONTROL_LOGS_V1" "$CONTROL_LOGS_V2" <<'PY'
import datetime
import json
import sys


def timestamp(value: str) -> datetime.datetime:
    return datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))


with open(sys.argv[1], encoding="utf-8") as source:
    legacy = json.load(source)
with open(sys.argv[2], encoding="utf-8") as source:
    current = json.load(source)

legacy_lines = legacy.get("logs")
legacy_entries = legacy.get("logEntries")
current_entries = current.get("logs")
if not legacy_lines or not legacy_entries or not current_entries:
    raise SystemExit("runtime logs were empty")

for item in legacy_lines:
    timestamp(item["timestamp"])
    line = json.loads(item["line"])
    if line.get("logger") != "a3s-box-runtime" or line.get("stream") not in {"stdout", "stderr"}:
        raise SystemExit(f"invalid legacy log line: {line!r}")

for item in [*legacy_entries, *current_entries]:
    timestamp(item["timestamp"])
    if item.get("level") not in {"debug", "info", "warn", "error"}:
        raise SystemExit(f"invalid log level: {item!r}")
    if not isinstance(item.get("message"), str):
        raise SystemExit(f"invalid log message: {item!r}")
    if item.get("fields", {}).get("stream") not in {"stdout", "stderr"}:
        raise SystemExit(f"invalid structured log fields: {item!r}")

legacy_times = [timestamp(item["timestamp"]) for item in legacy_entries]
current_times = [timestamp(item["timestamp"]) for item in current_entries]
if legacy_times != sorted(legacy_times):
    raise SystemExit("v1 logs were not ordered forward")
if current_times != sorted(current_times, reverse=True):
    raise SystemExit("v2 backward logs were not ordered backward")
PY
then
  fail 'control-plane runtime logs violated the pinned schemas or ordering'
fi

if [[ -n "$RUNTIME_IMAGE" ]]; then
  METRICS_RESPONSE="$STATE_DIR/envd-metrics.json"
  [[ "$(gateway_request "$DIRECT_HOST" "$METRICS_RESPONSE" X-Access-Token "$ENVD_TOKEN" /metrics)" == "200" ]] ||
    fail 'runtime envd metrics did not return HTTP 200'
  if ! python3 - "$METRICS_RESPONSE" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    metrics = json.load(source)
integer_fields = ("ts", "cpu_count", "mem_total", "mem_used", "disk_used", "disk_total")
for field in integer_fields:
    if type(metrics.get(field)) is not int or metrics[field] < 0:
        raise SystemExit(f"invalid non-negative integer metric {field!r}: {metrics.get(field)!r}")
if type(metrics.get("cpu_used_pct")) not in (int, float) or metrics["cpu_used_pct"] < 0:
    raise SystemExit(f"invalid cpu_used_pct: {metrics.get('cpu_used_pct')!r}")
if type(metrics.get("cpu_count")) is not int or metrics["cpu_count"] < 1:
    raise SystemExit(f"invalid cpu_count: {metrics.get('cpu_count')!r}")
PY
  then
    fail 'runtime envd metrics violated the pinned schema'
  fi

  ENVS_RESPONSE="$STATE_DIR/envd-envs.json"
  [[ "$(gateway_request "$DIRECT_HOST" "$ENVS_RESPONSE" X-Access-Token "$ENVD_TOKEN" /envs)" == "200" ]] ||
    fail 'runtime envd environment did not return HTTP 200'
  if ! python3 - "$ENVS_RESPONSE" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    environment = json.load(source)
if environment.get("SMOKE") != "true":
    raise SystemExit(f"unexpected SMOKE value: {environment.get('SMOKE')!r}")
PY
  then
    fail 'runtime envd environment omitted the create-time variable'
  fi

  ENVD_FILE_PATH='/tmp/a3s-box-envd-http-smoke.txt'
  ENVD_FILE_QUERY='/files?path=%2Ftmp%2Fa3s-box-envd-http-smoke.txt&username=user'
  ENVD_FILE_SOURCE="$STATE_DIR/envd-upload.txt"
  ENVD_UPLOAD_RESPONSE="$STATE_DIR/envd-upload.json"
  ENVD_DOWNLOAD_RESPONSE="$STATE_DIR/envd-download.txt"
  printf '%s' 'A3S Box runtime envd HTTP transfer' >"$ENVD_FILE_SOURCE"
  [[ "$(gateway_request "$DIRECT_HOST" "$ENVD_UPLOAD_RESPONSE" X-Access-Token "$ENVD_TOKEN" \
    "$ENVD_FILE_QUERY" --request POST \
    --header 'X-Metadata-A3S-Smoke: envd-http' \
    --form "file=@$ENVD_FILE_SOURCE;filename=a3s-box-envd-http-smoke.txt")" == "200" ]] ||
    fail 'runtime envd file upload did not return HTTP 200'
  if ! python3 - "$ENVD_UPLOAD_RESPONSE" "$ENVD_FILE_PATH" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    entries = json.load(source)
if not isinstance(entries, list) or len(entries) != 1:
    raise SystemExit(f"unexpected upload response: {entries!r}")
entry = entries[0]
if entry.get("path") != sys.argv[2] or entry.get("name") != "a3s-box-envd-http-smoke.txt":
    raise SystemExit(f"unexpected uploaded entry: {entry!r}")
if entry.get("type") != "file":
    raise SystemExit(f"unexpected uploaded entry type: {entry!r}")
if entry.get("metadata", {}).get("a3s-smoke") != "envd-http":
    raise SystemExit(f"uploaded metadata was not preserved: {entry!r}")
PY
  then
    fail 'runtime envd file upload violated the pinned schema'
  fi
  [[ "$(gateway_request "$DIRECT_HOST" "$ENVD_DOWNLOAD_RESPONSE" X-Access-Token "$ENVD_TOKEN" \
    "$ENVD_FILE_QUERY")" == "200" ]] ||
    fail 'runtime envd file download did not return HTTP 200'
  cmp --silent "$ENVD_FILE_SOURCE" "$ENVD_DOWNLOAD_RESPONSE" ||
    fail 'runtime envd file download differed from the uploaded content'
  [[ "$(gateway_request "$DIRECT_HOST" /dev/null X-Access-Token wrong-token /metrics)" == "401" ]] ||
    fail 'runtime envd metrics accepted an invalid token'

  CONTROL_METRICS_RESPONSE="$STATE_DIR/control-metrics.json"
  [[ "$(status_request GET "/sandboxes/metrics?sandbox_ids=$SANDBOX_ID" "$CONTROL_METRICS_RESPONSE")" == "200" ]] ||
    fail 'control-plane batch metrics did not return HTTP 200'
  if ! python3 - "$CONTROL_METRICS_RESPONSE" "$SANDBOX_ID" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    response = json.load(source)
metric = response.get("sandboxes", {}).get(sys.argv[2])
required = (
    "timestamp",
    "timestampUnix",
    "cpuCount",
    "cpuUsedPct",
    "memUsed",
    "memTotal",
    "memCache",
    "diskUsed",
    "diskTotal",
)
if not isinstance(metric, dict) or any(field not in metric for field in required):
    raise SystemExit(f"invalid batch metric: {metric!r}")
PY
  then
    fail 'control-plane batch metrics violated the pinned schema'
  fi
fi

TRAFFIC_HOST="49999-$SANDBOX_ID.$SANDBOX_DOMAIN"
[[ "$(gateway_status "$TRAFFIC_HOST" "$STATE_DIR/traffic-body.txt" E2B-Traffic-Access-Token "$TRAFFIC_TOKEN")" == "200" ]] ||
  fail 'TLS traffic route did not return HTTP 200'
[[ "$(cat "$STATE_DIR/traffic-body.txt")" == "$EXPECTED_TRAFFIC_BODY" ]] ||
  fail 'TLS traffic route returned the wrong Sandbox response body'
[[ "$(gateway_status "$TRAFFIC_HOST" /dev/null E2B-Traffic-Access-Token "$ENVD_TOKEN")" == "401" ]] ||
  fail 'TLS traffic route accepted an envd token'

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
[[ "$(gateway_status "$DIRECT_HOST" /dev/null X-Access-Token "$ENVD_TOKEN")" == "502" ]] ||
  fail 'authenticated envd health did not report the killed sandbox as stopped'
[[ "$(gateway_status "$DIRECT_HOST" /dev/null X-Access-Token wrong-token)" == "401" ]] ||
  fail 'terminal envd health accepted an invalid token'
[[ "$(gateway_status "$TRAFFIC_HOST" /dev/null E2B-Traffic-Access-Token "$TRAFFIC_TOKEN")" == "404" ]] ||
  fail 'stale TLS traffic route remained available after kill'

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

if [[ "${A3S_BOX_E2B_OFFICIAL_CLIENTS:-}" == "1" ]]; then
  [[ -f "$OFFICIAL_CLIENT_RUNNER" ]] ||
    fail "official-client runner is missing: $OFFICIAL_CLIENT_RUNNER"
  OFFICIAL_CLIENT_ARGS=(
    --api-url "$BASE_URL"
    --domain "$SANDBOX_DOMAIN"
    --template fixture-template
  )
  if [[ -n "${A3S_BOX_E2B_PIP_BOOTSTRAP_WHEEL:-}" ]]; then
    OFFICIAL_CLIENT_ARGS+=(
      --pip-bootstrap-wheel "$A3S_BOX_E2B_PIP_BOOTSTRAP_WHEEL"
    )
  fi
  if [[ -n "${A3S_BOX_E2B_ARTIFACT_CACHE:-}" ]]; then
    OFFICIAL_CLIENT_ARGS+=(--artifact-cache "$A3S_BOX_E2B_ARTIFACT_CACHE")
  fi
  if [[ "${A3S_BOX_E2B_NATIVE_SDKS:-}" == "1" ]]; then
    OFFICIAL_CLIENT_ARGS+=(--native-sdks)
  fi
  SMOKE_NO_PROXY="${NO_PROXY:+$NO_PROXY,}$SANDBOX_DOMAIN,.$SANDBOX_DOMAIN,127.0.0.1,localhost"
  E2B_API_KEY="$API_KEY" \
    SSL_CERT_FILE="$TLS_CERT" \
    NODE_EXTRA_CA_CERTS="$TLS_CERT" \
    NO_PROXY="$SMOKE_NO_PROXY" \
    no_proxy="$SMOKE_NO_PROXY" \
    "${A3S_BOX_E2B_OFFICIAL_PYTHON:-python3}" \
    "$OFFICIAL_CLIENT_RUNNER" "${OFFICIAL_CLIENT_ARGS[@]}"

  python3 - "$STATE_DIR/managed-executions.json" "$A3S_HOME" <<'PY'
import json
import pathlib
import sys

records_path = pathlib.Path(sys.argv[1])
home = pathlib.Path(sys.argv[2])
with records_path.open(encoding="utf-8") as source:
    records = json.load(source)
for record in records:
    execution_id = record["id"]
    if record.get("status") != "stopped":
        raise SystemExit(f"managed execution {execution_id} is not stopped")
    for path in (
        home / "boxes" / execution_id,
        home / "run" / "crun" / execution_id,
        pathlib.Path("/tmp/a3s-box-sockets") / execution_id,
    ):
        if path.exists():
            raise SystemExit(f"runtime resource leaked after official clients: {path}")
PY
fi

stop_service
printf 'E2B production smoke passed: lifecycle, envd HTTP, TLS traffic proxy, restart recovery, credentials, official clients when enabled, and cleanup\n'
