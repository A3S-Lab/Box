# Real Apple Silicon/HVF CI gate

The `integration-hvf` CI job qualifies the macOS published-port data path on a
physical Apple Silicon host. It boots a real Box MicroVM, performs two
sequential bidirectional connections through one published port, executes a
command after those connections, and rejects shim logs containing the fatal
`ENOBUFS` path or a guest `NETDEV WATCHDOG` timeout.

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
- registry access for the digest-pinned Alpine qualification image, or a
  configured registry mirror.

The workflow runs only for trusted non-pull-request events. It does not execute
arbitrary pull-request code on the persistent host.

## Enable the gate

Create these repository Actions variables:

| Variable | Value | Purpose |
| --- | --- | --- |
| `HVF_CI` | `true` | Enables the real Apple Silicon/HVF job. |
| `HVF_CI_REGISTRY_MIRRORS` | Optional Box mirror mapping | Redirects the qualification image pull on restricted networks. |

Every enabled run uploads a small evidence artifact containing the exact Box
and vendored libkrun revisions, host architecture, HVF availability, and the
test log. A successful source-only unit test or hosted macOS build is not a
substitute for this artifact.

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
```

