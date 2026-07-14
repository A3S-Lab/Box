# Official Client Lifecycle Fixtures

These fixtures execute the published, unmodified Python sync, Python async,
TypeScript, and Code Interpreter packages pinned by `upstream.lock.json`.
Their create, connect, list, timeout, kill, and not-found flows run against a
deterministic recording server.

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
