# Official Client Lifecycle Fixtures

These fixtures execute the published, unmodified Python sync, Python async,
TypeScript, and Code Interpreter packages pinned by `upstream.lock.json`.
Their create, connect, list, timeout, kill, and not-found flows run against a
deterministic recording server and the Rust lifecycle router.

The runner downloads the exact wheel and npm tarball URLs from the source lock,
verifies SHA-256 and npm integrity before installation, and records only stable
wire fields: method, path, query serialization, JSON body, authentication,
content type, and official user agent. It never disables the SDK API-key
validator; the fixture key uses the accepted `e2b_` plus lowercase hexadecimal
form.

Run with Python 3.10 or newer, Node.js 20 or newer, npm, and internet access:

```bash
python3 compat/e2b/fixtures/official-clients/run_fixtures.py generate
python3 compat/e2b/fixtures/official-clients/run_fixtures.py verify
```

The repository CI builds the Rust fixture server and makes the live router run
mandatory:

```bash
cd src
cargo build -p a3s-box-compat --bin a3s-box-e2b-fixture-server
cd ..
python3 compat/e2b/fixtures/official-clients/run_fixtures.py verify \
  --rust-server-bin src/target/debug/a3s-box-e2b-fixture-server
```

The runner uses `uv` when it is available on `PATH`, then falls back to the
standard-library `venv` module. On hosts where Python was packaged without
`ensurepip`, pass a trusted pip wheel without installing it into the host:

```bash
python3 compat/e2b/fixtures/official-clients/run_fixtures.py \
  --pip-bootstrap-wheel /path/to/pip.whl verify
```

`PIP_INDEX_URL`, `PIP_DEFAULT_TIMEOUT`, and `PIP_RETRIES` are honored when set.
The defaults are PyPI, 60 seconds, and five retries.

Use `--artifact-cache /path/to/cache` to reuse downloaded SDK wheels and npm
tarballs. Every cached artifact is checked against the SHA-256 and npm
integrity values in `upstream.lock.json` before use. Direct downloads use a
120-second timeout and retry up to three times.

Generated JSON Lines files are compatibility evidence, not server
implementations. The Rust control plane must satisfy them without adding A3S
fields to upstream requests or responses.

## Production lifecycle, health, and foreground-command gate

`run_production.py` installs the same checksum-pinned artifacts and runs the
unchanged Python sync, Python async, TypeScript, and Code Interpreter packages
against an already-running production compatibility service:

```bash
E2B_API_KEY=e2b_a1b2c3 \
python3 compat/e2b/fixtures/official-clients/run_production.py \
  --api-url http://127.0.0.1:38081 \
  --domain localhost.localdomain \
  --template fixture-template \
  --artifact-cache /path/to/verified-artifacts
```

On an A3S OS host, `scripts/e2b-production-smoke.sh` can make this matrix part
of the destructive real-Sandbox gate by setting
`A3S_BOX_E2B_OFFICIAL_CLIENTS=1`. Hosts without `ensurepip` can additionally
set `A3S_BOX_E2B_PIP_BOOTSTRAP_WHEEL`; the wheel is used through `PYTHONPATH`
and is not installed into the host Python environment.

Official-client data-plane calls use HTTPS, so the configured wildcard sandbox
domain must resolve to the gateway listener. Port `443` is the default. On a
host where another data plane reserves that port, set
`A3S_BOX_E2B_GATEWAY_SMOKE_PORT`; the smoke service advertises
`<domain>:<port>` in lifecycle responses, so envd, Code Interpreter, MCP, and
user-service URLs keep direct wildcard routing without `E2B_SANDBOX_URL`.
The production smoke defaults to `localhost.localdomain`, whose wildcard hosts
resolve on loopback while preserving normal TLS hostname validation. A DNS and
certificate preflight fails before any Sandbox is created when an override is
not routable.

The public-client matrix covers production lifecycle behavior, envd health,
Filesystem, foreground and background Process operations, stdin, PTY resize,
memory-preserving and filesystem-only pause, paused-state listing,
connect-based resume, warm-pause survival of the same background process,
cold-pause rootfs persistence and process replacement, environment
reinitialization, Volume remounting, Volume control and content operations,
bidirectional Sandbox mounts, UID/GID mapping, in-use deletion conflicts, Code
Interpreter execution and contexts, and cleanup. The enclosing smoke gate also
validates envd metrics/environment and HTTP file transfer directly through the
authenticated production data-plane route. The repository compatibility
manifest remains the source of truth for the versions and matrix that have
passed in production.
