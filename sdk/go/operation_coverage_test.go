package box

import (
	"context"
	"encoding/base64"
	"testing"

	"github.com/A3S-Lab/Box/sdk/go/v3/internal/bridge"
)

func TestPublicAPIExercisesEveryBridgeOperation(t *testing.T) {
	runtime := &fakeRuntime{handler: operationFixture}
	client := mustClient(runtime)
	ctx := context.Background()
	check := func(_ any, err error) {
		t.Helper()
		if err != nil {
			t.Fatal(err)
		}
	}

	check(client.RuntimeDiagnostics(ctx))
	check(client.RuntimeDiskUsage(ctx))
	check(client.Image(".").Tag("local/test:latest").Build(ctx))
	check(client.PullImage(
		ctx,
		"alpine:3.20",
		PullForce(),
		PullPlatform("linux/arm64"),
		PullCredentials(BasicCredentials("ci", "secret")),
		PullSignaturePolicy(SkipSignatures()),
	))
	check(client.GetImage(ctx, "alpine:3.20"))
	check(client.ListImages(ctx))
	check(client.InspectImage(ctx, "alpine:3.20"))
	check(client.ImageHistory(ctx, "alpine:3.20"))
	check(client.TagImage(ctx, "alpine:3.20", "local/alpine:latest"))
	check(client.PushImage(
		ctx,
		"local/alpine:latest",
		"registry/alpine:latest",
		PushCredentials(BasicCredentials("ci", "secret")),
		PushProtocol(RegistryHTTPS),
	))
	mustNoError(t, client.RemoveImage(ctx, "local/alpine:latest"))
	check(client.EvictImages(ctx))

	check(client.Volume("cache").Create(ctx))
	check(client.GetVolume(ctx, "cache"))
	check(client.ListVolumes(ctx))
	check(client.RemoveVolume(ctx, "cache", true))
	check(client.PruneVolumes(ctx))
	check(client.Network("ci-net").Create(ctx))
	check(client.GetNetwork(ctx, "ci-net"))
	check(client.ListNetworks(ctx))
	check(client.RemoveNetwork(ctx, "ci-net"))
	check(client.PruneNetworks(ctx))
	check(client.ListSandboxes(ctx, true))
	check(client.GetSandbox(ctx, "box-1"))

	sandbox, err := client.Sandbox(DefaultImage).Start(ctx)
	mustNoError(t, err)
	check(client.ConnectSandbox(ctx, "box-1"))
	mustNoError(t, sandbox.Stop(ctx))
	mustNoError(t, sandbox.Restart(ctx, RestartOperationID("coverage-restart")))
	mustNoError(t, sandbox.Pause(ctx, true))
	mustNoError(t, sandbox.Resume(ctx))
	check(sandbox.Logs(ctx, 10))
	check(sandbox.Stats(ctx))
	check(sandbox.CreateFilesystemSnapshot(ctx, "snap-1"))
	check(sandbox.Run(ctx, Argv("true")))
	files := sandbox.Files()
	check(files.WriteString(ctx, "/tmp/value", "value"))
	check(files.Read(ctx, "/tmp/value"))
	check(files.Stat(ctx, "/tmp/value"))
	check(files.List(ctx, "/tmp", 1))
	mustNoError(t, files.MakeDir(ctx, "/tmp/dir"))
	mustNoError(t, files.Move(ctx, "/tmp/dir", "/tmp/moved"))
	mustNoError(t, files.Remove(ctx, "/tmp/moved"))
	mustNoError(t, sandbox.Kill(ctx))

	removable := newSandbox(runtime, SandboxInfo{SandboxID: "box-remove", Generation: 1, State: StateStopped, Isolation: IsolationMicroVM})
	mustNoError(t, removable.Remove(ctx))
	check(client.ListFilesystemSnapshots(ctx))
	check(client.GetFilesystemSnapshot(ctx, "snap-1"))
	if _, _, err := client.FilesystemSnapshotSize(ctx, "snap-1"); err != nil {
		t.Fatal(err)
	}
	check(client.DeleteFilesystemSnapshot(ctx, "snap-1"))

	requests := runtime.Requests()
	for _, field := range []struct {
		operation string
		name      string
	}{
		{"image_build", "platforms"},
		{"sandbox_create", "ports"},
		{"sandbox_create", "dns"},
	} {
		request := requestByOperation(t, requests, field.operation)
		values, ok := request[field.name].([]any)
		if !ok || len(values) != 0 {
			t.Errorf("%s.%s must encode an empty JSON array, got %#v", field.operation, field.name, request[field.name])
		}
	}

	called := make(map[string]bool)
	for _, operation := range runtime.Operations() {
		called[operation] = true
	}
	for _, operation := range bridge.RequiredOperations {
		if !called[operation] {
			t.Errorf("public API did not exercise bridge operation %q", operation)
		}
	}
}

func operationFixture(_ context.Context, request map[string]any) (any, error) {
	switch request["operation"] {
	case "runtime_diagnostics":
		return RuntimeDiagnostics{CoreVersion: "3", Virtualization: RuntimeVirtualization{Available: true}}, nil
	case "runtime_disk_usage":
		return RuntimeDiskUsage{Home: "/tmp/a3s", TotalBytes: 1}, nil
	case "image_build":
		return BuildImageInfo{Reference: "local/test:latest"}, nil
	case "image_pull", "image_tag":
		return ImageInfo{Reference: "alpine:3.20"}, nil
	case "image_get":
		return map[string]any{"image": ImageInfo{Reference: "alpine:3.20"}}, nil
	case "image_list":
		return map[string]any{"images": []ImageInfo{{Reference: "alpine:3.20"}}}, nil
	case "image_inspect":
		return map[string]any{"image": ImageInspectInfo{ImageInfo: ImageInfo{Reference: "alpine:3.20"}}}, nil
	case "image_history":
		return map[string]any{"history": []ImageHistoryInfo{{CreatedBy: "RUN true"}}}, nil
	case "image_push":
		return PushImageInfo{Reference: "registry/alpine:latest"}, nil
	case "image_remove":
		return map[string]any{"removed": true}, nil
	case "image_evict":
		return map[string]any{"references": []string{"old:latest"}}, nil
	case "volume_create", "volume_remove":
		return VolumeInfo{Name: "cache"}, nil
	case "volume_get":
		return map[string]any{"volume": VolumeInfo{Name: "cache"}}, nil
	case "volume_list":
		return map[string]any{"volumes": []VolumeInfo{{Name: "cache"}}}, nil
	case "volume_prune":
		return map[string]any{"names": []string{"old-cache"}}, nil
	case "network_create", "network_remove":
		return NetworkInfo{Name: "ci-net", Subnet: "10.89.0.0/24"}, nil
	case "network_get":
		return map[string]any{"network": NetworkInfo{Name: "ci-net"}}, nil
	case "network_list":
		return map[string]any{"networks": []NetworkInfo{{Name: "ci-net"}}}, nil
	case "network_prune":
		return map[string]any{"names": []string{"old-net"}}, nil
	case "sandbox_list":
		return map[string]any{"sandboxes": []SandboxSummary{{ID: "box-1"}}}, nil
	case "sandbox_get":
		return map[string]any{"sandbox": SandboxSummary{ID: "box-1"}}, nil
	case "sandbox_create", "sandbox_inspect":
		return SandboxInfo{SandboxID: "box-1", Generation: 1, State: StateRunning, Isolation: IsolationMicroVM}, nil
	case "sandbox_stop":
		return SandboxInfo{SandboxID: "box-1", Generation: 1, State: StateStopped, Isolation: IsolationMicroVM}, nil
	case "sandbox_restart":
		return SandboxInfo{SandboxID: "box-1", Generation: 2, State: StateRunning, Isolation: IsolationMicroVM}, nil
	case "sandbox_pause":
		return SandboxInfo{SandboxID: "box-1", Generation: 2, State: StatePaused, Isolation: IsolationMicroVM}, nil
	case "sandbox_resume":
		return SandboxInfo{SandboxID: "box-1", Generation: 2, State: StateRunning, Isolation: IsolationMicroVM}, nil
	case "sandbox_logs":
		return map[string]any{"logs": []SandboxLogEntry{}}, nil
	case "sandbox_stats":
		return map[string]any{"stats": SandboxStats{ID: "box-1"}}, nil
	case "sandbox_snapshot_create":
		return FilesystemSnapshotInfo{SnapshotID: "snap-1", Generation: 2}, nil
	case "sandbox_kill":
		return SandboxInfo{SandboxID: "box-1", Generation: 2, State: StateFailed, Isolation: IsolationMicroVM}, nil
	case "sandbox_remove":
		return SandboxInfo{SandboxID: "box-remove", Generation: 1, State: StateRemoved, Isolation: IsolationMicroVM}, nil
	case "command_run":
		return map[string]any{"stdout_base64": "", "stderr_base64": "", "exit_code": 0, "truncated": false}, nil
	case "file_write":
		return WriteInfo{Path: "/tmp/value", Size: 5}, nil
	case "file_read":
		return map[string]any{"data_base64": base64.StdEncoding.EncodeToString([]byte("value")), "size": 5}, nil
	case "filesystem_stat":
		return map[string]any{"entry": EntryInfo{Path: "/tmp/value", Type: "file"}}, nil
	case "filesystem_list":
		return map[string]any{"entries": []EntryInfo{}}, nil
	case "filesystem_make_dir", "filesystem_move", "filesystem_remove":
		return map[string]any{"ok": true}, nil
	case "filesystem_snapshot_list":
		return map[string]any{"snapshots": []FilesystemSnapshotSummary{{ID: "snap-1"}}}, nil
	case "filesystem_snapshot_get":
		return map[string]any{"snapshot": FilesystemSnapshotSummary{ID: "snap-1"}}, nil
	case "filesystem_snapshot_size":
		return map[string]any{"snapshot_id": "snap-1", "size_bytes": uint64(5)}, nil
	case "filesystem_snapshot_delete":
		return map[string]any{"snapshot_id": "snap-1", "deleted": true}, nil
	default:
		return map[string]any{}, nil
	}
}

func mustNoError(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}
