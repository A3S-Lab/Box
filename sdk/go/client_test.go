package box

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"reflect"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/A3S-Lab/Box/sdk/go/v3/internal/bridge"
)

func TestOperationInventoryMatchesRustContract(t *testing.T) {
	payload, err := os.ReadFile("../bridge-operations.json")
	if err != nil {
		t.Fatal(err)
	}
	var inventory []string
	if err := json.Unmarshal(payload, &inventory); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(inventory, bridge.RequiredOperations) {
		t.Fatalf("Go operation inventory differs from Rust contract\nGo:   %v\nRust: %v", bridge.RequiredOperations, inventory)
	}
	seen := make(map[string]struct{}, len(inventory))
	for _, operation := range inventory {
		if _, duplicate := seen[operation]; duplicate {
			t.Fatalf("duplicate operation %q", operation)
		}
		seen[operation] = struct{}{}
	}
}

func TestNewClientFailsClosedBeforeMutation(t *testing.T) {
	operations := append([]string(nil), bridge.RequiredOperations...)
	operations = slices.DeleteFunc(operations, func(operation string) bool {
		return operation == "sandbox_create"
	})
	runtime := &fakeRuntime{handler: func(_ context.Context, request map[string]any) (any, error) {
		if request["operation"] == "sdk_capabilities" {
			return Capabilities{ProtocolVersion: bridge.ProtocolVersion, Operations: operations}, nil
		}
		return nil, errors.New("mutation must not run")
	}}
	// Override the helper's automatic capability response for this test.
	runtimeWithMissing := runtimeFunc(func(ctx context.Context, request any, result any) error {
		payload, _ := json.Marshal(request)
		var decoded map[string]any
		_ = json.Unmarshal(payload, &decoded)
		runtime.mu.Lock()
		runtime.requests = append(runtime.requests, decoded)
		runtime.mu.Unlock()
		return assignResult(result, Capabilities{ProtocolVersion: bridge.ProtocolVersion, Operations: operations})
	})
	client, err := NewClient(context.Background(), WithRuntime(runtimeWithMissing))
	if client != nil || !errors.Is(err, ErrUnavailable) {
		t.Fatalf("expected unavailable handshake failure, client=%v err=%v", client, err)
	}
	if !strings.Contains(err.Error(), "sandbox_create") {
		t.Fatalf("missing operation is absent from error: %v", err)
	}
	if got := len(runtime.Requests()); got != 1 {
		t.Fatalf("expected exactly one non-mutating request, got %d", got)
	}
}

func TestNewClientRejectsCapabilityProtocolMismatch(t *testing.T) {
	runtime := runtimeFunc(func(_ context.Context, _ any, result any) error {
		return assignResult(result, Capabilities{
			ProtocolVersion: bridge.ProtocolVersion + 1,
			Operations:      bridge.RequiredOperations,
		})
	})
	_, err := NewClient(context.Background(), WithRuntime(runtime))
	if !errors.Is(err, ErrProtocol) {
		t.Fatalf("expected protocol error, got %v", err)
	}
}

func TestBuildersValidateBeforeMutation(t *testing.T) {
	runtime := &fakeRuntime{}
	client := mustClient(runtime)
	baseline := len(runtime.Requests())

	if _, err := client.Image("").Build(context.Background()); !errors.Is(err, ErrInvalidRequest) {
		t.Fatalf("expected invalid image builder, got %v", err)
	}
	if _, err := client.Volume("").Create(context.Background()); !errors.Is(err, ErrInvalidRequest) {
		t.Fatalf("expected invalid volume builder, got %v", err)
	}
	if _, err := client.Network("ci").Subnet("not-cidr").Create(context.Background()); !errors.Is(err, ErrInvalidRequest) {
		t.Fatalf("expected invalid network builder, got %v", err)
	}
	if _, err := client.Sandbox("alpine:3.20").CPUs(0).Start(context.Background()); !errors.Is(err, ErrInvalidRequest) {
		t.Fatalf("expected invalid sandbox builder, got %v", err)
	}
	if got := len(runtime.Requests()); got != baseline {
		t.Fatalf("validation issued %d unexpected requests", got-baseline)
	}
}

func TestRegistryCredentialsDoNotFormatPassword(t *testing.T) {
	credentials := BasicCredentials("ci", "super-secret")
	for _, formatted := range []string{
		fmt.Sprint(credentials),
		fmt.Sprintf("%+v", credentials),
		fmt.Sprintf("%#v", credentials),
	} {
		if strings.Contains(formatted, "super-secret") {
			t.Fatalf("credential formatting exposed password: %s", formatted)
		}
	}
	if credentials.Username() != "ci" {
		t.Fatalf("unexpected credential username %q", credentials.Username())
	}
}

func TestProgrammableBuildersEncodeTypedConfiguration(t *testing.T) {
	runtime := &fakeRuntime{handler: func(_ context.Context, request map[string]any) (any, error) {
		switch request["operation"] {
		case "image_build":
			return BuildImageInfo{Reference: "local/ci:latest", Digest: "sha256:1"}, nil
		case "volume_create":
			return VolumeInfo{Name: "go-cache", SizeLimit: 4096}, nil
		case "network_create":
			return NetworkInfo{Name: "ci-net", Subnet: "10.44.0.0/24"}, nil
		case "sandbox_create":
			return SandboxInfo{SandboxID: "box-1", Generation: 1, State: StateRunning}, nil
		default:
			return map[string]any{}, nil
		}
	}}
	client := mustClient(runtime)
	ctx := context.Background()

	image, err := client.Image("./ci").
		Dockerfile("Dockerfile.ci").
		Tag("local/ci:latest").
		BuildArg("GO_VERSION", "1.25").
		Platform("linux/arm64").
		Target("test").
		NoCache(true).
		Build(ctx)
	if err != nil || image.Reference != "local/ci:latest" {
		t.Fatalf("build image: %+v, %v", image, err)
	}
	volume, err := client.Volume("go-cache").Label("scope", "ci").SizeLimit(4096).Create(ctx)
	if err != nil || volume.Name != "go-cache" {
		t.Fatalf("create volume: %+v, %v", volume, err)
	}
	network, err := client.Network("ci-net").Subnet("10.44.0.0/24").Label("scope", "ci").Create(ctx)
	if err != nil || network.Name != "ci-net" {
		t.Fatalf("create network: %+v, %v", network, err)
	}
	sandbox, err := client.Sandbox(image.Reference).
		Timeout(90*time.Second).
		Env("CI", "true").
		Label("job", "test").
		Name("go-test").
		CPUs(4).
		MemoryMiB(4096).
		Isolation(IsolationSandbox).
		FilesystemSnapshot("base-snapshot").
		Workspace("/workspace").
		Workdir("/workspace/src").
		User("1000:1000").
		Hostname("go-ci").
		Mount(NamedVolume(volume.Name, "/go/pkg/mod").ReadOnly()).
		Mount(BindMount("./src", "/workspace/src")).
		Tmpfs(Tmpfs("/tmp").SizeBytes(1024)).
		Network(BridgeNetwork(network.Name)).
		PublishTCP(0, 8080).
		DNSServer("1.1.1.1").
		HostAlias("registry.local", "10.44.0.2").
		ReadOnly(true).
		Persistent(true).
		AutoRemove(false).
		Start(ctx)
	if err != nil || sandbox.ID() != "box-1" {
		t.Fatalf("start sandbox: %v, %v", sandbox, err)
	}

	requests := runtime.Requests()
	build := requestByOperation(t, requests, "image_build")
	if build["quiet"] != true || build["target"] != "test" {
		t.Fatalf("unexpected image build request: %#v", build)
	}
	if platforms, ok := build["platforms"].([]any); !ok || len(platforms) != 1 {
		t.Fatalf("image platforms must be a JSON array: %#v", build["platforms"])
	}
	create := requestByOperation(t, requests, "sandbox_create")
	if create["timeout_seconds"] != float64(90) || create["isolation"] != string(IsolationSandbox) {
		t.Fatalf("unexpected sandbox request: %#v", create)
	}
	mounts, ok := create["mounts"].([]any)
	if !ok || len(mounts) != 2 {
		t.Fatalf("unexpected mounts: %#v", create["mounts"])
	}
}

func requestByOperation(t *testing.T, requests []map[string]any, operation string) map[string]any {
	t.Helper()
	for _, request := range requests {
		if request["operation"] == operation {
			return request
		}
	}
	t.Fatalf("operation %q was not requested", operation)
	return nil
}

type runtimeFunc func(context.Context, any, any) error

func (function runtimeFunc) Request(ctx context.Context, request any, result any) error {
	return function(ctx, request, result)
}

func assignResult(target, value any) error {
	payload, err := json.Marshal(value)
	if err != nil {
		return err
	}
	return json.Unmarshal(payload, target)
}
