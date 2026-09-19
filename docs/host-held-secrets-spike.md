# Host-held secret substitution — design spike (not a product)

Status: **not implemented**. This note exists so the next networking change does not invent TLS interception or retire Compose `secret_environment`.

Axis C item 4 in `architecture-optimization-plan.md`. Out of scope: replacing tmpfs secrets, claiming MITM, CNI, or closing B3/B2.

## What ships today

Linux Compose `secret_environment` keeps secret bytes out of ACL, `.env`, `BoxConfig`, labels, and state records. Box materializes them on a caller-owned private tmpfs, mounts that tree read-only at `/.a3s-box-secrets`, and guest init injects the values into the workload environment (`exec_config::materialize_secret_environment`). The guest process therefore holds the secret bytes. Other hosts parse the references and reject execution.

netproxy / passt_bridge terminate TCP for proxy and DNS. They do not parse TLS, do not see SNI policy, and do not rewrite HTTP bodies.

## Optional path (not scheduled as code)

A host-held path would be complementary, not a replacement:

1. Guest sees a placeholder, not the secret.
2. Substitution happens only on a host-terminated TLS connection whose SNI (and resolved address) is on an explicit allow-list.
3. The host opens the upstream TLS session itself and injects the secret on that session only.

## Why this is not the next code change

- Certificate pinning, custom trust stores, and QUIC/HTTP3 bypass a TCP TLS terminator. Advertising “host-held secrets” while those paths still carry guest-held bytes would be a false claim.
- Default untrusted egress (`#580`) already denies loopback, link-local/metadata, and foreign private ranges. A TLS substituter must not weaken that filter to reach a secret store.
- Deprecating tmpfs `secret_environment` before the TLS path is proven on a real workload removes a shipped §4.2 row.

## Decision

Do not add a TLS MITM, SNI allow-list, or secret-rewrite API in this repository until a concrete workload cannot use tmpfs injection and the bypasses above are either closed or documented as fail-closed. Until then, `secret_environment` remains the product secret path.
