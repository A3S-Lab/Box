# A3S Box Go SDK

The Go SDK is a typed, local-first API for A3S Box. It builds OCI images,
manages volumes and networks, configures Sandboxes with fluent builders, runs
commands and stdin-backed scripts, and exposes binary-safe filesystem and
snapshot operations.

It talks only to the installed `a3s-box sdk-bridge` process. There is no remote
service configuration, account setup, or credential required to use a local
runtime. `A3S_BOX_BINARY` is optional and only selects a non-default binary.

## Install

Install the A3S Box runtime first and verify it on the target machine:

```bash
curl --proto '=https' --tlsv1.2 -fsSL \
  https://raw.githubusercontent.com/A3S-Lab/Box/main/install.sh | sh

a3s-box --version
a3s-box info
```

Then add the Go module:

```bash
go get github.com/A3S-Lab/Box/sdk/go/v3
```

Use the SDK and runtime from the same A3S Box release. `NewClient` performs a
protocol v3 and exact 48-operation capability handshake before it permits any
mutation. Missing or duplicate operations fail closed.

## Quick start

```go
package main

import (
	"context"
	"fmt"
	"log"

	box "github.com/A3S-Lab/Box/sdk/go/v3"
)

func main() {
	ctx := context.Background()
	sandbox, err := box.Create(ctx, "alpine:3.20")
	if err != nil {
		log.Fatal(err)
	}
	defer sandbox.Close(context.Background())

	result, err := sandbox.Run(ctx, box.Argv("printf", "hello from A3S Box"))
	if err != nil {
		log.Fatal(err)
	}
	fmt.Println(result.StdoutString())

	if _, err := sandbox.Files().WriteString(ctx, "/tmp/note.txt", "hello"); err != nil {
		log.Fatal(err)
	}
}
```

Use `box.Argv(...)` for direct execution. Use `box.Shell(...)` only when shell
syntax is intentional.

## Initial process configuration

By default, `Sandbox(...).Start` replaces the image command with a long-running
keepalive process so `Run`, `Script`, and file operations remain available.
Use `Entrypoint` and `Command` to configure the initial OCI process explicitly:

```go
sandbox, err := client.
	Sandbox("alpine:3.20").
	Entrypoint("/bin/sh", "-c").
	Command("echo ready; exec httpd -f -p 8080").
	PublishTCP(8080, 8080).
	Start(ctx)
```

This is also the supported way to launch application work on Windows/WHPX,
where post-boot command execution is currently unavailable. Empty argument
vectors and blank first elements fail locally before the runtime is invoked.

## Programmable CI/CD

Builders provide code-first image, cache, network, Sandbox, and script
configuration without introducing a separate workflow format:

```go
ctx := context.Background()
client, err := box.NewClient(ctx)
if err != nil {
	return err
}

image, err := client.
	Image("./ci").
	Dockerfile("Dockerfile").
	Tag("local/go-ci:latest").
	BuildArg("GO_VERSION", "1.25").
	Build(ctx)
if err != nil {
	return err
}

cache, err := client.
	Volume("go-cache").
	Label("purpose", "dependency-cache").
	Create(ctx)
if err != nil {
	return err
}

network, err := client.
	Network("ci-network").
	Subnet("10.89.20.0/24").
	Create(ctx)
if err != nil {
	return err
}

sandbox, err := client.
	Sandbox(image.Reference).
	CPUs(4).
	MemoryMiB(4096).
	Mount(box.NamedVolume(cache.Name, "/go/pkg/mod")).
	Network(box.BridgeNetwork(network.Name)).
	Start(ctx)
if err != nil {
	return err
}
defer sandbox.Close(context.Background())

result, err := sandbox.
	Script("go test ./...\n").
	Interpreter("/bin/sh", "-se").
	Env("CI", "true").
	Directory("/workspace").
	Run(ctx)
if err != nil {
	return err
}
if result.ExitCode != 0 {
	return fmt.Errorf("tests failed: %s", result.StderrString())
}
```

Use `box.NoNetwork()` for a network-disabled Sandbox and `box.TSINetwork()` for
the default transparent socket interception mode. Bind mounts, named volumes,
tmpfs, bridge networks, ports, DNS servers, host aliases, snapshot restore,
initial command and entrypoint overrides, read-only roots, persistence, and
automatic cleanup are all typed builder values.

## API surface

| Area | Go API |
| --- | --- |
| Runtime | `NewClient`, `Capabilities`, `RuntimeDiagnostics`, `RuntimeDiskUsage` |
| Images | `Image(...).Build`, `PullImage`, `GetImage`, `ListImages`, `InspectImage`, `ImageHistory`, `TagImage`, `PushImage`, `RemoveImage`, `EvictImages` |
| Volumes | `Volume(...).Create`, `GetVolume`, `ListVolumes`, `RemoveVolume`, `PruneVolumes` |
| Networks | `Network(...).Create`, `GetNetwork`, `ListNetworks`, `RemoveNetwork`, `PruneNetworks` |
| Sandbox | `Create`, `Connect`, `Sandbox(...).Command(...).Entrypoint(...).Start`, `Inspect`, `Stop`, `Restart`, `Pause`, `Resume`, `Kill`, `Remove`, `Close` |
| Execution | `Run`, `Commands().Run`, `Script`, `ScriptBytes` |
| Files | `Write`, `WriteString`, `Read`, `ReadString`, `Stat`, `Exists`, `List`, `MakeDir`, `Move`, `Remove` |
| Snapshots | `CreateFilesystemSnapshot`, `ListFilesystemSnapshots`, `GetFilesystemSnapshot`, `FilesystemSnapshotSize`, `DeleteFilesystemSnapshot` |
| Observability | `ListSandboxes`, `GetSandbox`, `Logs`, `Stats`, `IsRunning` |

All runtime I/O accepts `context.Context`. Command and file calls on one
`Sandbox` may run concurrently. Lifecycle transitions are serialized against
in-flight calls and update the generation fence atomically. `Close` is bounded,
idempotent after successful cleanup, and retryable after a cleanup failure.

Command output and file primitives use `[]byte`. The `StdoutString`,
`StderrString`, `WriteString`, and `ReadString` helpers are conveniences for
text workloads.

## Errors and cancellation

Failures use `*box.Error` with a stable `ErrorCode`. Match categories with
`errors.Is` and inspect details with `errors.As`:

```go
result, err := sandbox.Run(ctx, box.Argv("make", "test"))
if errors.Is(err, context.DeadlineExceeded) {
	return fmt.Errorf("job deadline exceeded: %w", err)
}
if errors.Is(err, box.ErrConflict) {
	return fmt.Errorf("Sandbox generation changed: %w", err)
}
_ = result
```

The SDK preserves `context.Canceled` and `context.DeadlineExceeded` through
error wrapping. A caller deadline remains `context.DeadlineExceeded`, while the
runtime process ceiling is `ErrBridgeTimeout`. Stable SDK categories include
`ErrInvalidRequest`, `ErrNotFound`, `ErrConflict`, `ErrUnavailable`,
`ErrRuntime`, `ErrProtocol`, `ErrBinaryNotFound`, and `ErrBridgeTimeout`.
`ErrNotInstalled` remains a deprecated alias for source compatibility.

## Runtime selection

The default runtime resolves `A3S_BOX_BINARY` and then `a3s-box` on `PATH`:

```bash
export A3S_BOX_BINARY=/opt/a3s/bin/a3s-box
go run ./cmd/worker
```

Applications that need an explicit typed runtime can configure it directly:

```go
runtime := box.NewLocalRuntime(
	box.WithBinaryPath("/opt/a3s/bin/a3s-box"),
	box.WithBridgeTimeout(5*time.Minute),
)
client, err := box.NewClient(ctx, box.WithRuntime(runtime))
```

Registry credentials, when used for image pull or push, are encoded in the
bridge request on stdin and are never placed in process arguments.

## Development and release tags

```bash
gofmt -w .
go vet ./...
go test ./...
go test -race ./...
```

This is a nested Go module. A repository release `vX.Y.Z` is published to the
Go module ecosystem with the matching path-prefixed tag
`sdk/go/vX.Y.Z`. The release workflow creates and verifies that tag after the
tested GitHub release succeeds.
