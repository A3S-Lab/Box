# Real Apple Silicon/HVF CI gate

The `integration-hvf` CI job qualifies macOS runtime behavior on a physical
Apple Silicon host. It boots real Box MicroVMs and:

- performs two sequential bidirectional connections through one published
  port, executes a command afterwards, and rejects shim logs containing the
  fatal `ENOBUFS` path or a guest `NETDEV WATCHDOG` timeout;
- sends `SIGTERM` to an attached `run --rm`, proves cleanup, and recovers exit
  status 143 from the removal archive by both name and ID; and
- runs the reduced pnpm fixture through a digest-pinned Node image, Corepack,
  the persistent pnpm cache, outbound registry traffic, tmpfs `node_modules`,
  and a command-wide timeout.

GitHub-hosted arm64 macOS runners cannot run this job because they do not expose
nested virtualization. The gate therefore stays skipped until a trusted
self-hosted runner is enrolled and the repository variable is enabled.

## Enroll the runner

Register a physical Apple Silicon Mac as a self-hosted GitHub Actions runner
with these labels:

```text
self-hosted, macos, arm64, hvf
```

The runner account needs:

- Hypervisor.framework access (`sysctl -n kern.hv_support` must return `1`);
- Rust and the `aarch64-unknown-linux-musl` target;
- Zig and `cargo-zigbuild` for the Linux guest-init cross-build;
- Xcode command-line tools, CMake, LLVM, protobuf, and pkg-config; and
- registry access for the digest-pinned Alpine and Node qualification images,
  plus npm registry access, or configured mirrors.

The workflow runs only for trusted non-pull-request events. It does not execute
arbitrary pull-request code on the persistent host.

## Enable the gate

Create these repository Actions variables:

| Variable | Value | Purpose |
| --- | --- | --- |
| `HVF_CI` | `true` | Enables the real Apple Silicon/HVF job. |
| `HVF_CI_REGISTRY_MIRRORS` | Optional Box mirror mapping | Redirects the qualification image pull on restricted networks. |

Every enabled run uploads an evidence artifact containing the exact Box and
vendored libkrun revisions, host architecture, HVF availability, published-port
and interruption test logs, and the bounded pnpm qualification logs. A
successful source-only unit test or hosted macOS build is not a substitute for
this artifact. The reduced pnpm fixture is a platform regression gate; issues
that explicitly require a larger production checkout still need that separate
record before closure.

## Run the regression manually

From the repository root on a physical Apple Silicon Mac:

```bash
unset A3S_DEPS_STUB
rustup target add aarch64-unknown-linux-musl
cd src
cargo zigbuild --locked --release -p a3s-box-guest-init \
  --target aarch64-unknown-linux-musl
cargo build --locked --release -p a3s-box-cli -p a3s-box-shim
codesign --entitlements shim/entitlements.plist --force -s - \
  target/release/a3s-box-shim
A3S_BOX_ALLOW_REGISTRY_PULL=1 \
  A3S_BOX_SMOKE_TIMEOUT_SECS=600 \
  cargo test --locked --release -p a3s-box-cli --test core_smoke \
    real_core_tsi_published_tcp_nonblocking_accept_and_exec \
    -- --ignored --nocapture --test-threads=1
cargo test --locked --release -p a3s-box-cli --test core_smoke \
  real_core_foreground_auto_remove_handles_sigterm_and_archives_status \
  -- --ignored --nocapture --test-threads=1
cd ..
A3S_BOX="$PWD/src/target/release/a3s-box" \
  PNPM_PROJECT="$PWD/bench/fixtures/pnpm" \
  PNPM_IMAGE='docker.io/library/node:22-alpine@sha256:c610fcdfb1d5b4740dd70c284ed3cb16bb857e0f7166196e36a5501df7a3aa32' \
  PNPM_RUNS=1 PNPM_TIMEOUT=600 PNPM_NODE_MODULES=tmpfs \
  bash bench/bench.sh pnpm
```
