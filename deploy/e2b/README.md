# E2B Runtime OCI Template

This directory defines the OCI image used by an A3S Box template whose envd
service runs inside a `--isolation sandbox` execution. The image is built by
GitHub Actions; it is not intended to be built on developer workstations.

The build pins:

- E2B infra commit `fda7bef1095afb909197e272c0a8a123797f0bfb` and envd `0.6.9`;
- Code Interpreter commit `5aeca43fe3fae2df260b1fb17c71fed5b5dac852`;
- the JavaScript kernel commit declared in the Dockerfile.

The image starts envd on port `49983`, initializes its default user and working
directory, and starts the Code Interpreter service on port `49999`. A production
ACL template selects the in-Sandbox daemon explicitly:

```acl
template_policy "code-interpreter-v1" {
  image        = "ghcr.io/a3s-lab/box-e2b-runtime:<immutable-tag>"
  envd_version = "0.6.9"
  envd_mode    = "runtime"
  isolation    = "sandbox"

  resources {
    vcpus    = 2
    memory_mb = 2048
    disk_mb   = 8192
  }

  route {
    port        = 49983
    token_scope = "envd"
  }

  route {
    port        = 49999
    token_scope = "traffic"
  }
}
```

Use an immutable image tag or digest in production. Edge access tokens are
validated by the A3S compatibility gateway and are not forwarded into the
Sandbox service.
