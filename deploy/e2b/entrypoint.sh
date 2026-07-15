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

/usr/local/bin/envd -isnotfc -no-cgroups &
children+=("$!")

/usr/bin/python3 /usr/local/lib/a3s-box-e2b/init-envd.py

runuser -u user -- env \
  HOME=/home/user \
  PATH="${PATH}" \
  /opt/a3s/e2b/jupyter/bin/jupyter server \
  --IdentityProvider.token= \
  --ServerApp.root_dir=/home/user &
children+=("$!")

for attempt in {1..150}; do
  if curl --fail --silent --output /dev/null http://127.0.0.1:8888/api/status; then
    break
  fi
  if ((attempt == 150)); then
    echo "Jupyter did not become healthy within 30 seconds" >&2
    exit 1
  fi
  sleep 0.2
done

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

set +e
wait -n "${children[@]}"
status=$?
set -e
echo "An E2B runtime service exited with status ${status}" >&2
exit "${status}"
