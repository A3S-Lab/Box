#!/usr/bin/env bash
#
# install-runtimeclass.sh — provision a Kubernetes node to run RuntimeClass=a3s-box pods.
#
# Run as root ON each node that should host a3s-box MicroVM workloads. It installs:
#   * the a3s-box CLI + helpers (a3s-box, a3s-box-cri, a3s-box-guest-init, a3s-box-shim)
#   * libkrun / libkrunfw (the MicroVM VMM, into the system lib dir + ldconfig)
#   * the containerd runtime-v2 shim (containerd-shim-a3s-box-v2)
# and registers the `io.containerd.a3s-box.v2` runtime with containerd, then restarts it.
#
# After running this on a node, label it from a control-plane so the RuntimeClass
# nodeSelector (a3s-box.io/runtime=true) lets a3s-box pods schedule there:
#
#     kubectl label node <node-name> a3s-box.io/runtime=true
#
# Usage:
#   install-runtimeclass.sh [--version vX.Y.Z] [--repo OWNER/REPO] [--from-dir DIR]
#
#   --version   release tag to install                 (default: v3.0.12)
#   --repo      GitHub repo to download artifacts from (default: A3S-Lab/Box)
#   --from-dir  install from a local directory instead of downloading; the dir must
#               contain a3s-box-<version>-linux-<arch>.tar.gz, its .sha256 file,
#               and a containerd shim binary
#               (containerd-shim-a3s-box-v2[-linux-<arch>]).
#               The checksum manifest always names the suffixed release asset,
#               even when the local shim file uses the unsuffixed fallback name.
#
# Idempotent: safe to re-run (re-installs binaries, rewrites the containerd drop-in).
set -euo pipefail

VERSION="v3.0.12"
REPO="A3S-Lab/Box"
FROM_DIR=""
WARMUP_IMAGE="busybox:latest"   # first box on a fresh node builds a one-time cache
                                # (~40s+); booting one here primes it so the first
                                # real pod doesn't exceed the shim's boot poll. Best
                                # effort — skipped silently if the image can't pull.

while [ $# -gt 0 ]; do
  case "$1" in
    --version)      VERSION="$2"; shift 2 ;;
    --repo)         REPO="$2";    shift 2 ;;
    --from-dir)     FROM_DIR="$2"; shift 2 ;;
    --warmup-image) WARMUP_IMAGE="$2"; shift 2 ;;
    --no-warmup)    WARMUP_IMAGE=""; shift ;;
    -h|--help)  sed -n '2,33p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31mERROR:\033[0m %s\n' "$*" >&2; exit 1; }

# A non-empty root is used only by the hermetic installer self-test. Production
# invocations leave it empty and therefore retain the documented system paths.
INSTALL_ROOT="${A3S_INSTALL_ROOT:-}"
RESTART_DELAY="${A3S_INSTALL_RESTART_DELAY:-2}"
root_path() { printf '%s%s' "${INSTALL_ROOT%/}" "$1"; }
BIN_DIR="$(root_path /usr/local/bin)"
LIB_DIR="$(root_path /usr/lib)"
CONTAINERD_BIN_DIR="$(root_path /opt/containerd/bin)"
STATE_DIR="$(root_path /var/lib/a3s-box)"
CONTAINERD_CONFIG="$(root_path /etc/containerd/config.toml)"
CONTAINERD_CONF_DIR="$(root_path /etc/containerd/conf.d)"
DEV_KVM="$(root_path /dev/kvm)"

# ── preflight ───────────────────────────────────────────────────────────────
if [ -z "$INSTALL_ROOT" ]; then
  [ "$(id -u)" = 0 ] || die "must run as root"
fi
command -v containerd >/dev/null || die "containerd not found on this node"
[ -e "$DEV_KVM" ] || die "$DEV_KVM missing — this node has no KVM virtualization; a3s-box cannot run here"

case "$(uname -m)" in
  x86_64)        ARCH=x86_64 ;;
  aarch64|arm64) ARCH=arm64  ;;
  *) die "unsupported architecture: $(uname -m)" ;;
esac

TARBALL="a3s-box-${VERSION}-linux-${ARCH}.tar.gz"
SHIM_ASSET="containerd-shim-a3s-box-v2-linux-${ARCH}"
CHECKSUM_ASSET="a3s-box-${VERSION}-linux-${ARCH}.sha256"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# ── obtain artifacts ────────────────────────────────────────────────────────
if [ -n "$FROM_DIR" ]; then
  log "Installing from local dir: $FROM_DIR"
  [ -f "$FROM_DIR/$TARBALL" ] || die "missing $TARBALL in $FROM_DIR"
  [ -f "$FROM_DIR/$CHECKSUM_ASSET" ] || die "missing $CHECKSUM_ASSET in $FROM_DIR"
  cp "$FROM_DIR/$TARBALL" "$work/$TARBALL"
  cp "$FROM_DIR/$CHECKSUM_ASSET" "$work/$CHECKSUM_ASSET"
  if   [ -f "$FROM_DIR/$SHIM_ASSET" ];                 then cp "$FROM_DIR/$SHIM_ASSET" "$work/$SHIM_ASSET"
  elif [ -f "$FROM_DIR/containerd-shim-a3s-box-v2" ];  then cp "$FROM_DIR/containerd-shim-a3s-box-v2" "$work/$SHIM_ASSET"
  else die "missing containerd-shim-a3s-box-v2 in $FROM_DIR"; fi
else
  base="https://github.com/${REPO}/releases/download/${VERSION}"
  log "Downloading $TARBALL"
  curl -fsSL "$base/$TARBALL" -o "$work/$TARBALL" || die "download failed: $base/$TARBALL"
  log "Downloading $SHIM_ASSET"
  curl -fsSL "$base/$SHIM_ASSET" -o "$work/$SHIM_ASSET" || die "download failed: $base/$SHIM_ASSET"
  log "Downloading $CHECKSUM_ASSET"
  curl -fsSL "$base/$CHECKSUM_ASSET" -o "$work/$CHECKSUM_ASSET" || die "download failed: $base/$CHECKSUM_ASSET"
fi

log "Verifying release checksums"
manifest_entry_count=0
manifest_tarball_count=0
manifest_shim_count=0
while IFS= read -r checksum_line || [ -n "$checksum_line" ]; do
  [ -n "$checksum_line" ] || continue
  if [[ "$checksum_line" =~ ^[[:xdigit:]]{64}[[:space:]]+\*?(.+)$ ]]; then
    checksum_name="${BASH_REMATCH[1]}"
  else
    die "invalid checksum manifest line in $CHECKSUM_ASSET"
  fi
  manifest_entry_count=$((manifest_entry_count + 1))
  case "$checksum_name" in
    "$TARBALL") manifest_tarball_count=$((manifest_tarball_count + 1)) ;;
    "$SHIM_ASSET") manifest_shim_count=$((manifest_shim_count + 1)) ;;
    *) die "$CHECKSUM_ASSET contains unexpected path: $checksum_name" ;;
  esac
done < "$work/$CHECKSUM_ASSET"
[ "$manifest_entry_count" -eq 2 ] &&
  [ "$manifest_tarball_count" -eq 1 ] &&
  [ "$manifest_shim_count" -eq 1 ] ||
  die "$CHECKSUM_ASSET must contain exactly one checksum for $TARBALL and $SHIM_ASSET"
(cd "$work" && sha256sum --strict --check "$CHECKSUM_ASSET") ||
  die "release checksum verification failed"

tar xzf "$work/$TARBALL" -C "$work"
src="$work/a3s-box-${VERSION}-linux-${ARCH}"
[ -d "$src" ] || die "unexpected tarball layout (no $src)"

# ── install binaries + libkrun ──────────────────────────────────────────────
log "Installing a3s-box binaries to $BIN_DIR"
install -d "$BIN_DIR"
install -m0755 "$src/a3s-box" "$src/a3s-box-cri" "$src/a3s-box-guest-init" "$src/a3s-box-shim" "$BIN_DIR/"

log "Installing libkrun to $LIB_DIR"
install -d "$LIB_DIR"
cp -a "$src"/lib/libkrun* "$LIB_DIR/"
if [ -z "$INSTALL_ROOT" ]; then
  ldconfig
fi

log "Installing containerd-shim-a3s-box-v2 ($BIN_DIR + $CONTAINERD_BIN_DIR)"
install -m0755 "$work/$SHIM_ASSET" "$BIN_DIR/containerd-shim-a3s-box-v2"
install -d "$CONTAINERD_BIN_DIR"
install -m0755 "$work/$SHIM_ASSET" "$CONTAINERD_BIN_DIR/containerd-shim-a3s-box-v2"

install -d "$STATE_DIR"   # shared A3S_HOME for the shim's a3s-box invocations

# ── register the runtime with containerd ────────────────────────────────────
config_before="$work/containerd-config-before.toml"
containerd config dump > "$config_before" || die "containerd config dump failed"
CONTAINERD_CONFIG_VERSION="$(
  sed -n 's/^[[:space:]]*version[[:space:]]*=[[:space:]]*\([0-9][0-9]*\).*$/\1/p' \
    "$config_before" | head -n 1
)"
case "$CONTAINERD_CONFIG_VERSION" in
  2) PLUGIN_NAMESPACE="io.containerd.grpc.v1.cri" ;;
  3) PLUGIN_NAMESPACE="io.containerd.cri.v1.runtime" ;;
  *) die "unsupported containerd config version: ${CONTAINERD_CONFIG_VERSION:-missing}" ;;
esac
log "Detected containerd config v$CONTAINERD_CONFIG_VERSION ($PLUGIN_NAMESPACE)"

runtime_block() {
  cat <<TOML
[plugins.'$PLUGIN_NAMESPACE'.containerd.runtimes.a3s-box]
  runtime_type = 'io.containerd.a3s-box.v2'
  [plugins.'$PLUGIN_NAMESPACE'.containerd.runtimes.a3s-box.options]
TOML
}

has_runtime_table() {
  [ -f "$1" ] || return 1
  sed 's/[[:space:]"'"'"']//g' "$1" |
    grep -Fqx "[plugins.$PLUGIN_NAMESPACE.containerd.runtimes.a3s-box]"
}

install -d "$(dirname "$CONTAINERD_CONFIG")"
if [ ! -f "$CONTAINERD_CONFIG" ]; then
  printf 'version = %s\n' "$CONTAINERD_CONFIG_VERSION" > "$CONTAINERD_CONFIG"
fi
if grep -qE "^[[:space:]]*imports[[:space:]]*=.*conf\.d" "$CONTAINERD_CONFIG" 2>/dev/null; then
  # containerd merges /etc/containerd/conf.d/*.toml — register via a drop-in so we
  # never touch the main config (clean + idempotent).
  install -d "$CONTAINERD_CONF_DIR"
  {
    printf 'version = %s\n\n' "$CONTAINERD_CONFIG_VERSION"
    runtime_block
  } > "$CONTAINERD_CONF_DIR/a3s-box.toml"
  log "Registered runtime via $CONTAINERD_CONF_DIR/a3s-box.toml"
elif has_runtime_table "$CONTAINERD_CONFIG"; then
  log "Runtime already present in $CONTAINERD_CONFIG — leaving as-is"
else
  # No conf.d imports: append the version-correct runtime table to the main config.
  { echo; runtime_block; } >> "$CONTAINERD_CONFIG"
  log "Registered runtime in $CONTAINERD_CONFIG"
fi

log "Restarting containerd"
systemctl restart containerd
sleep "$RESTART_DELAY"

# ── verify ──────────────────────────────────────────────────────────────────
log "Verification"
systemctl is-active --quiet containerd || die "containerd is not active after restart"
config_after="$work/containerd-config-after.toml"
containerd config dump > "$config_after" || die "containerd config dump failed after restart"
awk -v target="[plugins.$PLUGIN_NAMESPACE.containerd.runtimes.a3s-box]" '
  function normalize(value) {
    gsub(/[[:space:]]/, "", value)
    gsub(/"/, "", value)
    gsub(/\047/, "", value)
    return value
  }
  normalize($0) == target { in_runtime = 1; found_table = 1; next }
  /^\[/ { in_runtime = 0 }
  in_runtime && normalize($0) == "runtime_type=io.containerd.a3s-box.v2" {
    found_type = 1
  }
  END { exit !(found_table && found_type) }
' "$config_after" ||
  die "containerd config dump does not contain the a3s-box handler in $PLUGIN_NAMESPACE"
"$BIN_DIR/a3s-box" --version >/dev/null 2>&1 || die "a3s-box CLI not runnable"
echo "  a3s-box:   $("$BIN_DIR/a3s-box" --version 2>/dev/null)"
if [ -n "$INSTALL_ROOT" ]; then
  echo "  libkrun:   $(find "$LIB_DIR" -maxdepth 1 -name 'libkrun.so*' -print -quit)"
else
  echo "  libkrun:   $(ldconfig -p | awk '/libkrun\.so/{print $1; exit}')"
fi
echo "  shim:      $BIN_DIR/containerd-shim-a3s-box-v2"
echo "  /dev/kvm:  $DEV_KVM"
echo "  containerd: active"

# ── warm up (prime the one-time per-node boot cache) ────────────────────────
if [ -n "$WARMUP_IMAGE" ]; then
  log "Warming up with $WARMUP_IMAGE (primes first-boot cache; --no-warmup to skip)"
  if A3S_HOME="$STATE_DIR" timeout 240 "$BIN_DIR/a3s-box" run \
        --name a3sbox-warmup "$WARMUP_IMAGE" -- true >/dev/null 2>&1; then
    echo "  warm-up OK — first pod will boot fast"
  else
    echo "  warm-up skipped (could not pull $WARMUP_IMAGE) — first pod may cold-start slowly"
  fi
  A3S_HOME="$STATE_DIR" "$BIN_DIR/a3s-box" rm -f a3sbox-warmup >/dev/null 2>&1 || true
fi

printf '\n\033[1;32mDone.\033[0m a3s-box runtime installed on %s.\n' "$(hostname)"
cat <<EOF
Final step — from a control-plane node, label this node so a3s-box pods can schedule:

    kubectl label node $(hostname) a3s-box.io/runtime=true

EOF
