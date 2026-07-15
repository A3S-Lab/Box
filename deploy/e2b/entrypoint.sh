#!/bin/bash
set -euo pipefail

children=()

terminate() {
  if ((${#children[@]})); then
    kill -TERM "${children[@]}" 2>/dev/null || true
    wait "${children[@]}" 2>/dev/null || true
  fi
}
trap terminate EXIT INT TERM

wait_for_service() {
  local name="$1"
  local url="$2"
  local pid="$3"

  for attempt in {1..150}; do
    if curl --fail --silent --output /dev/null "${url}"; then
      return 0
    fi
    if ! kill -0 "${pid}" 2>/dev/null; then
      local status=0
      wait "${pid}" || status=$?
      echo "${name} exited before becoming healthy with status ${status}" >&2
      if ((status == 0)); then
        status=1
      fi
      return "${status}"
    fi
    sleep 0.2
  done

  echo "${name} did not become healthy within 30 seconds" >&2
  return 1
}

runuser -u user -- env \
  HOME=/home/user \
  PATH="${PATH}" \
  /opt/a3s/e2b/jupyter/bin/jupyter server \
  --IdentityProvider.token= \
  --ServerApp.root_dir=/home/user &
children+=("$!")
jupyter_pid="$!"

wait_for_service "Jupyter" "http://127.0.0.1:8888/api/status" "${jupyter_pid}"

runuser -u user -- env \
  HOME=/home/user \
  PATH="${PATH}" \
  /opt/a3s/e2b/code-interpreter/.venv/bin/uvicorn \
  main:app \
  --app-dir /opt/a3s/e2b/code-interpreter \
  --host 0.0.0.0 \
  --port 49999 \
  --workers 1 \
  --no-access-log \
  --no-use-colors \
  --timeout-keep-alive 640 &
children+=("$!")
code_interpreter_pid="$!"

wait_for_service \
  "Code Interpreter" \
  "http://127.0.0.1:49999/health" \
  "${code_interpreter_pid}"

/usr/local/bin/envd -isnotfc -no-cgroups &
children+=("$!")

/usr/bin/python3 /usr/local/lib/a3s-box-e2b/init-envd.py

set +e
wait -n "${children[@]}"
status=$?
set -e
echo "An E2B runtime service exited with status ${status}" >&2
exit "${status}"
