#!/usr/bin/env bash
# CI wrapper around prepare-linux-sandbox-host.sh --ci.
# Kept as a stable entrypoint for .github/workflows/ci.yml.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec bash "${SCRIPT_DIR}/prepare-linux-sandbox-host.sh" --ci "$@"
