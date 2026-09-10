#!/usr/bin/env bash
set -euo pipefail
cd /mnt/d/code/a3s/crates/box
git add CHANGELOG.md README.md ROADMAP.md src/runtime/Cargo.toml \
  scripts/linux-native-live-session-qualification.sh \
  src/runtime/examples/linux_native_live_session_qualification.rs
cat > /tmp/box-commit-msg.txt <<'EOF'
feat(qualification): add Native Linux live-session Host-reopen harness

Add an observation-only Sandbox gate that requires supervised create, SIGKILLs
the Native Linux Host owner while a generation is live, rebinds, and continues
authentic Live state/inventory/stats/kill without inventing exit status.
Honestly scopes Native Linux only; KVM MicroVM Live and utility-VM remain open.
EOF
git commit -F /tmp/box-commit-msg.txt
git status -sb
git rev-parse HEAD
git log -1 --format='%an %ae%n%s'
