package box

import (
	"context"
	"encoding/base64"
	"errors"
	"io"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

func TestSandboxLifecycleTracksGenerationAndState(t *testing.T) {
	runtime := &fakeRuntime{handler: func(_ context.Context, request map[string]any) (any, error) {
		operation := request["operation"]
		switch operation {
		case "sandbox_inspect":
			return SandboxInfo{SandboxID: "box-1", Generation: 1, State: StateRunning, Isolation: IsolationSandbox}, nil
		case "sandbox_stop":
			assertGeneration(t, request, 1)
			return SandboxInfo{SandboxID: "box-1", Generation: 2, State: StateStopped, Isolation: IsolationSandbox}, nil
		case "sandbox_restart":
			assertGeneration(t, request, 2)
			if !strings.HasPrefix(stringValue(request["operation_id"]), "sdk-restart-") {
				t.Fatalf("restart operation ID was not generated: %#v", request)
			}
			return SandboxInfo{SandboxID: "box-1", Generation: 3, State: StateRunning, Isolation: IsolationSandbox}, nil
		case "sandbox_pause":
			assertGeneration(t, request, 3)
			return SandboxInfo{SandboxID: "box-1", Generation: 3, State: StatePaused, Isolation: IsolationSandbox}, nil
		case "sandbox_resume":
			assertGeneration(t, request, 3)
			return SandboxInfo{SandboxID: "box-1", Generation: 3, State: StateRunning, Isolation: IsolationSandbox}, nil
		case "sandbox_logs":
			return map[string]any{"logs": []map[string]any{{"stream": "stdout", "log": "ready"}}}, nil
		case "sandbox_stats":
			return map[string]any{"stats": map[string]any{"id": "box-1", "pid": 42, "cpus": 2}}, nil
		case "sandbox_snapshot_create":
			return FilesystemSnapshotInfo{SnapshotID: "snap-1", SizeBytes: 12, State: StateRunning, Generation: 3}, nil
		case "sandbox_kill":
			assertGeneration(t, request, 3)
			return SandboxInfo{SandboxID: "box-1", Generation: 3, State: StateFailed, Isolation: IsolationSandbox}, nil
		default:
			t.Fatalf("unexpected operation: %v", operation)
			return nil, nil
		}
	}}
	sandbox := newSandbox(runtime, SandboxInfo{
		SandboxID: "box-1", Generation: 1, State: StateRunning, Isolation: IsolationSandbox,
	})
	ctx := context.Background()
	if sandbox.Isolation() != IsolationSandbox {
		t.Fatalf("isolation=%s", sandbox.Isolation())
	}

	running, err := sandbox.IsRunning(ctx)
	if err != nil || !running {
		t.Fatalf("is running: %v, %v", running, err)
	}
	if err := sandbox.Stop(ctx); err != nil || sandbox.Generation() != 2 || sandbox.State() != StateStopped {
		t.Fatalf("stop state=%s generation=%d err=%v", sandbox.State(), sandbox.Generation(), err)
	}
	if err := sandbox.Restart(ctx, RestartStopTimeout(2*time.Second)); err != nil {
		t.Fatal(err)
	}
	if sandbox.Generation() != 3 || sandbox.State() != StateRunning {
		t.Fatalf("restart state=%s generation=%d", sandbox.State(), sandbox.Generation())
	}
	if err := sandbox.Pause(ctx, false); err != nil || sandbox.State() != StatePaused {
		t.Fatalf("pause state=%s err=%v", sandbox.State(), err)
	}
	if err := sandbox.Resume(ctx); err != nil || sandbox.State() != StateRunning {
		t.Fatalf("resume state=%s err=%v", sandbox.State(), err)
	}
	logs, err := sandbox.Logs(ctx, 10)
	if err != nil || len(logs) != 1 || logs[0].Message != "ready" {
		t.Fatalf("logs=%+v err=%v", logs, err)
	}
	stats, err := sandbox.Stats(ctx)
	if err != nil || stats == nil || stats.PID != 42 {
		t.Fatalf("stats=%+v err=%v", stats, err)
	}
	snapshot, err := sandbox.CreateFilesystemSnapshot(ctx, "snap-1")
	if err != nil || snapshot.SnapshotID != "snap-1" {
		t.Fatalf("snapshot=%+v err=%v", snapshot, err)
	}
	if err := sandbox.Kill(ctx); err != nil || sandbox.State() != StateKilled {
		t.Fatalf("kill state=%s err=%v", sandbox.State(), err)
	}
	beforeClose := len(runtime.Requests())
	if err := sandbox.Close(ctx); err != nil {
		t.Fatal(err)
	}
	if got := len(runtime.Requests()); got != beforeClose {
		t.Fatalf("idempotent close issued another request")
	}
}

func TestSandboxRuntimeControlUsesExactGeneration(t *testing.T) {
	pid := uint32(42)
	waitTimeoutMS := uint64(50)
	runtime := &fakeRuntime{handler: func(_ context.Context, request map[string]any) (any, error) {
		switch request["operation"] {
		case "sandbox_processes":
			assertGeneration(t, request, 3)
			return ExecutionProcessInventory{
				ExecutionID: "box-1",
				Generation:  3,
				Processes: []ExecutionProcessInfo{
					{ProcessID: "init", PID: &pid},
					{ProcessID: "exec-1", Terminal: true},
				},
			}, nil
		case "sandbox_runtime_stats":
			assertGeneration(t, request, 3)
			peak := uint64(1536)
			limit := uint64(2048)
			return ExecutionStats{
				ExecutionID:     "box-1",
				Generation:      3,
				TimestampUnixNS: 1_700_000_000_000_000_123,
				CPU: ExecutionCPUStats{
					UsageNS:     300,
					UserNS:      200,
					SystemNS:    100,
					ThrottledNS: 5,
				},
				Memory: ExecutionMemoryStats{
					UsageBytes: 1024,
					LimitBytes: &limit,
					PeakBytes:  &peak,
				},
				ProcessCount: 2,
				Metrics:      map[string]uint64{"io.read_bytes": 64},
			}, nil
		case "sandbox_events":
			assertGeneration(t, request, 3)
			if request["after_sequence"] != float64(7) || request["limit"] != float64(DefaultExecutionEventBatchLimit) || request["wait_timeout_ms"] != float64(50) {
				t.Fatalf("unexpected event request: %#v", request)
			}
			processID := "exec-1"
			return ExecutionEventBatch{
				ExecutionID: "box-1",
				Generation:  3,
				Events: []ExecutionRuntimeEvent{{
					Sequence:        8,
					TimestampUnixNS: 1_700_000_000_000_000_124,
					ProcessID:       &processID,
					Kind:            EventProcessExited,
					Attributes:      map[string]string{"exit_code": "0"},
				}},
				NextSequence: 8,
			}, nil
		case "sandbox_update_resources":
			assertGeneration(t, request, 3)
			if request["operation_id"] != "go-resources-1" {
				t.Fatalf("unexpected operation ID: %#v", request)
			}
			resources, ok := request["resources"].(map[string]any)
			if !ok || resources["cpu_shares"] != float64(512) || resources["cpuset_cpus"] != "0-1" {
				t.Fatalf("unexpected resource update: %#v", request)
			}
			return SandboxInfo{SandboxID: "box-1", Generation: 3, State: StateRunning, Isolation: IsolationSandbox}, nil
		default:
			t.Fatalf("unexpected operation: %v", request["operation"])
			return nil, nil
		}
	}}
	sandbox := newSandbox(runtime, SandboxInfo{
		SandboxID: "box-1", Generation: 3, State: StateRunning, Isolation: IsolationSandbox,
	})
	ctx := context.Background()

	processes, err := sandbox.Processes(ctx)
	if err != nil || len(processes.Processes) != 2 || processes.Processes[0].PID == nil || *processes.Processes[0].PID != 42 {
		t.Fatalf("processes=%+v err=%v", processes, err)
	}
	stats, err := sandbox.RuntimeStats(ctx)
	if err != nil ||
		stats.TimestampUnixNS != 1_700_000_000_000_000_123 ||
		stats.CPU.UsageNS != 300 ||
		stats.Metrics["io.read_bytes"] != 64 {
		t.Fatalf("stats=%+v err=%v", stats, err)
	}
	events, err := sandbox.Events(ctx, ExecutionEventsRequest{AfterSequence: 7, WaitTimeoutMS: &waitTimeoutMS})
	if err != nil ||
		len(events.Events) != 1 ||
		events.Events[0].TimestampUnixNS != 1_700_000_000_000_000_124 ||
		events.Events[0].Kind != EventProcessExited ||
		events.NextSequence != 8 {
		t.Fatalf("events=%+v err=%v", events, err)
	}
	cpuShares := uint64(512)
	cpuset := "0-1"
	if err := sandbox.UpdateResources(
		ctx,
		ExecutionResourceUpdate{CPUShares: &cpuShares, CPUSetCPUs: &cpuset},
		UpdateResourcesOperationID("go-resources-1"),
	); err != nil {
		t.Fatal(err)
	}

	before := len(runtime.Requests())
	invalidShares := uint64(1)
	invalidPIDs := uint64(0)
	invalidSwap := int64(-2)
	invalidCPUSet := "2-1"
	for _, update := range []ExecutionResourceUpdate{
		{},
		{CPUShares: &invalidShares},
		{PIDsLimit: &invalidPIDs},
		{MemorySwap: &invalidSwap},
		{CPUSetCPUs: &invalidCPUSet},
	} {
		if err := sandbox.UpdateResources(ctx, update); !errors.Is(err, ErrInvalidRequest) {
			t.Fatalf("update=%+v error=%v", update, err)
		}
	}
	if _, err := sandbox.Events(ctx, ExecutionEventsRequest{Limit: MaxExecutionEventBatchItems + 1}); !errors.Is(err, ErrInvalidRequest) {
		t.Fatalf("invalid event limit error=%v", err)
	}
	if err := sandbox.UpdateResources(
		ctx,
		ExecutionResourceUpdate{CPUShares: &cpuShares},
		UpdateResourcesOperationID(" "),
	); !errors.Is(err, ErrInvalidRequest) {
		t.Fatalf("blank operation ID error=%v", err)
	}
	if got := len(runtime.Requests()); got != before {
		t.Fatalf("invalid control request reached bridge: before=%d after=%d", before, got)
	}
}

func TestSandboxRemoveAndConnect(t *testing.T) {
	runtime := &fakeRuntime{handler: func(_ context.Context, request map[string]any) (any, error) {
		switch request["operation"] {
		case "sandbox_inspect":
			return SandboxInfo{SandboxID: "box-2", Generation: 4, State: StateStopped, Isolation: IsolationMicroVM}, nil
		case "sandbox_remove":
			assertGeneration(t, request, 4)
			return SandboxInfo{SandboxID: "box-2", Generation: 4, State: StateRemoved, Isolation: IsolationMicroVM}, nil
		default:
			return map[string]any{}, nil
		}
	}}
	client := mustClient(runtime)
	sandbox, err := client.ConnectSandbox(context.Background(), "box-2")
	if err != nil || sandbox.Generation() != 4 {
		t.Fatalf("connect=%v err=%v", sandbox, err)
	}
	if err := sandbox.Remove(context.Background()); err != nil || sandbox.State() != StateRemoved {
		t.Fatalf("remove state=%s err=%v", sandbox.State(), err)
	}
	before := len(runtime.Requests())
	if err := sandbox.Remove(context.Background()); err != nil || len(runtime.Requests()) != before {
		t.Fatalf("remove should be idempotent: %v", err)
	}
}

func TestConnectRejectsMissingIsolation(t *testing.T) {
	runtime := &fakeRuntime{handler: func(_ context.Context, request map[string]any) (any, error) {
		if request["operation"] == "sandbox_inspect" {
			return SandboxInfo{SandboxID: "box-2", Generation: 4, State: StateStopped}, nil
		}
		return map[string]any{}, nil
	}}
	client := mustClient(runtime)
	sandbox, err := client.ConnectSandbox(context.Background(), "box-2")
	if sandbox != nil || !errors.Is(err, ErrProtocol) {
		t.Fatalf("expected protocol error for missing isolation, sandbox=%v err=%v", sandbox, err)
	}
}

func TestConnectRejectsMalformedSandboxIdentityAndState(t *testing.T) {
	tests := []struct {
		name string
		info SandboxInfo
	}{
		{
			name: "missing sandbox ID",
			info: SandboxInfo{Generation: 4, State: StateStopped, Isolation: IsolationMicroVM},
		},
		{
			name: "different sandbox ID",
			info: SandboxInfo{SandboxID: "box-other", Generation: 4, State: StateStopped, Isolation: IsolationMicroVM},
		},
		{
			name: "zero generation",
			info: SandboxInfo{SandboxID: "box-2", State: StateStopped, Isolation: IsolationMicroVM},
		},
		{
			name: "unknown state",
			info: SandboxInfo{SandboxID: "box-2", Generation: 4, State: "unknown", Isolation: IsolationMicroVM},
		},
		{
			name: "unknown isolation",
			info: SandboxInfo{SandboxID: "box-2", Generation: 4, State: StateStopped, Isolation: "process"},
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			runtime := &fakeRuntime{handler: func(_ context.Context, request map[string]any) (any, error) {
				if request["operation"] == "sandbox_inspect" {
					return test.info, nil
				}
				return map[string]any{}, nil
			}}
			client := mustClient(runtime)
			sandbox, err := client.ConnectSandbox(context.Background(), "box-2")
			if sandbox != nil || !errors.Is(err, ErrProtocol) {
				t.Fatalf("expected protocol error, sandbox=%v err=%v", sandbox, err)
			}
		})
	}
}

func TestCloseFailureRemainsRetryable(t *testing.T) {
	var attempts atomic.Int32
	runtime := &fakeRuntime{handler: func(_ context.Context, request map[string]any) (any, error) {
		if request["operation"] != "sandbox_kill" {
			return nil, errors.New("unexpected operation")
		}
		if attempts.Add(1) == 1 {
			return nil, sdkError("sandbox_kill", CodeUnavailable, "runtime busy", nil)
		}
		return SandboxInfo{SandboxID: "box-1", Generation: 1, State: StateFailed, Isolation: IsolationMicroVM}, nil
	}}
	sandbox := newSandbox(runtime, SandboxInfo{SandboxID: "box-1", Generation: 1, State: StateRunning, Isolation: IsolationMicroVM})
	if err := sandbox.Close(context.Background()); !errors.Is(err, ErrUnavailable) {
		t.Fatalf("expected first cleanup failure, got %v", err)
	}
	if sandbox.State() != StateRunning {
		t.Fatalf("failed cleanup changed state to %s", sandbox.State())
	}
	if err := sandbox.Close(context.Background()); err != nil {
		t.Fatalf("retry failed: %v", err)
	}
	if attempts.Load() != 2 {
		t.Fatalf("expected two cleanup attempts, got %d", attempts.Load())
	}
}

func TestLifecycleWaitsForInFlightCommand(t *testing.T) {
	commandStarted := make(chan struct{})
	releaseCommand := make(chan struct{})
	killReachedRuntime := make(chan struct{})
	runtime := &fakeRuntime{handler: func(_ context.Context, request map[string]any) (any, error) {
		switch request["operation"] {
		case "command_run":
			close(commandStarted)
			<-releaseCommand
			return map[string]any{"stdout_base64": "", "stderr_base64": "", "exit_code": 0, "truncated": false}, nil
		case "sandbox_kill":
			close(killReachedRuntime)
			return SandboxInfo{SandboxID: "box-1", Generation: 1, State: StateFailed, Isolation: IsolationMicroVM}, nil
		default:
			return map[string]any{}, nil
		}
	}}
	sandbox := newSandbox(runtime, SandboxInfo{SandboxID: "box-1", Generation: 1, State: StateRunning, Isolation: IsolationMicroVM})
	commandDone := make(chan error, 1)
	go func() {
		_, err := sandbox.Run(context.Background(), Argv("true"))
		commandDone <- err
	}()
	<-commandStarted
	killDone := make(chan error, 1)
	go func() { killDone <- sandbox.Kill(context.Background()) }()
	select {
	case <-killReachedRuntime:
		t.Fatal("lifecycle request raced with an in-flight command")
	case <-time.After(40 * time.Millisecond):
	}
	close(releaseCommand)
	if err := <-commandDone; err != nil {
		t.Fatal(err)
	}
	if err := <-killDone; err != nil {
		t.Fatal(err)
	}
}

func TestCommandsScriptsAndFilesystemAreBinarySafe(t *testing.T) {
	binaryOutput := []byte{0xff, 0x00, 'A'}
	runtime := &fakeRuntime{handler: func(_ context.Context, request map[string]any) (any, error) {
		switch request["operation"] {
		case "command_run":
			return map[string]any{
				"stdout_base64": base64.StdEncoding.EncodeToString(binaryOutput),
				"stderr_base64": base64.StdEncoding.EncodeToString([]byte("warning")),
				"exit_code":     7,
				"truncated":     true,
			}, nil
		case "file_write":
			data, err := base64.StdEncoding.DecodeString(stringValue(request["data_base64"]))
			if err != nil || !reflect.DeepEqual(data, binaryOutput) {
				t.Fatalf("unexpected file data: %v, %v", data, err)
			}
			return WriteInfo{Path: stringValue(request["path"]), Size: uint64(len(data))}, nil
		case "file_read":
			return map[string]any{"path": request["path"], "data_base64": base64.StdEncoding.EncodeToString(binaryOutput), "size": 3}, nil
		case "filesystem_stat":
			if request["path"] == "/missing" {
				return nil, sdkError("filesystem_stat", CodeNotFound, "missing", nil)
			}
			return map[string]any{"entry": EntryInfo{Name: "data.bin", Type: "file", Path: "/data.bin", Size: 3}}, nil
		case "filesystem_list":
			return map[string]any{"entries": []EntryInfo{{Name: "data.bin", Type: "file", Path: "/data.bin"}}}, nil
		case "filesystem_make_dir", "filesystem_move", "filesystem_remove":
			return map[string]any{"ok": true}, nil
		default:
			return nil, errors.New("unexpected operation")
		}
	}}
	sandbox := newSandbox(runtime, SandboxInfo{SandboxID: "box-1", Generation: 9, State: StateRunning, Isolation: IsolationMicroVM})
	ctx := context.Background()

	result, err := sandbox.Run(
		ctx,
		Argv("printf", "value with spaces"),
		RunTimeout(1500*time.Millisecond),
		RunEnv("A", "B"),
		RunDirectory("/workspace"),
		RunAs("1000"),
		RunStdin(binaryOutput),
	)
	if err != nil || !reflect.DeepEqual(result.Stdout, binaryOutput) || result.ExitCode != 7 || !result.Truncated {
		t.Fatalf("command result=%+v err=%v", result, err)
	}
	command := requestByOperation(t, runtime.Requests(), "command_run")
	if command["timeout_ms"] != float64(1500) || command["generation"] != float64(9) {
		t.Fatalf("unexpected command request: %#v", command)
	}

	files := sandbox.Files()
	write, err := files.Write(ctx, "/data.bin", binaryOutput, FileAs("1000"))
	if err != nil || write.Size != 3 {
		t.Fatalf("write=%+v err=%v", write, err)
	}
	read, err := files.Read(ctx, "/data.bin")
	if err != nil || !reflect.DeepEqual(read, binaryOutput) {
		t.Fatalf("read=%v err=%v", read, err)
	}
	entry, err := files.Stat(ctx, "/data.bin")
	if err != nil || entry.Type != "file" {
		t.Fatalf("stat=%+v err=%v", entry, err)
	}
	exists, err := files.Exists(ctx, "/missing")
	if err != nil || exists {
		t.Fatalf("exists=%v err=%v", exists, err)
	}
	entries, err := files.List(ctx, "/", 2)
	if err != nil || len(entries) != 1 {
		t.Fatalf("list=%+v err=%v", entries, err)
	}
	if err := files.MakeDir(ctx, "/new"); err != nil {
		t.Fatal(err)
	}
	if err := files.Move(ctx, "/new", "/renamed"); err != nil {
		t.Fatal(err)
	}
	if err := files.Remove(ctx, "/renamed"); err != nil {
		t.Fatal(err)
	}

	scriptResult, err := sandbox.Script("echo script").
		Interpreter("/bin/bash", "-se").
		Env("CI", "1").
		Directory("/workspace").
		User("1000").
		Timeout(time.Second).
		Run(ctx)
	if err != nil || scriptResult.ExitCode != 7 {
		t.Fatalf("script=%+v err=%v", scriptResult, err)
	}
	requests := runtime.Requests()
	lastCommand := requests[len(requests)-1]
	argv, ok := lastCommand["argv"].([]any)
	if !ok || len(argv) != 2 || argv[0] != "/bin/bash" {
		t.Fatalf("unexpected script interpreter: %#v", lastCommand)
	}
	source, err := base64.StdEncoding.DecodeString(stringValue(lastCommand["stdin_base64"]))
	if err != nil || string(source) != "echo script" {
		t.Fatalf("unexpected script stdin: %q, %v", source, err)
	}
}

func TestArtifactExportIsBoundedHashedAndDoesNotOverwrite(t *testing.T) {
	runtime := &fakeRuntime{handler: func(_ context.Context, request map[string]any) (any, error) {
		switch request["operation"] {
		case "filesystem_stat":
			return map[string]any{"entry": EntryInfo{Name: "artifact.bin", Type: "file", Path: stringValue(request["path"]), Size: 5}}, nil
		case "file_read":
			return map[string]any{"path": request["path"], "data_base64": base64.StdEncoding.EncodeToString([]byte("hello")), "size": 5}, nil
		default:
			return nil, errors.New("unexpected operation")
		}
	}}
	sandbox := newSandbox(runtime, SandboxInfo{SandboxID: "box-1", Generation: 9, State: StateRunning, Isolation: IsolationMicroVM})
	destination := filepath.Join(t.TempDir(), "artifact.bin")

	artifact, err := sandbox.Files().Export(
		context.Background(),
		"/workspace/artifact.bin",
		ArtifactMaxBytes(5),
		ArtifactTo(destination),
		ArtifactAs("1000"),
	)
	if err != nil {
		t.Fatal(err)
	}
	if artifact.Path != "/workspace/artifact.bin" || string(artifact.Data) != "hello" || artifact.Size != 5 {
		t.Fatalf("unexpected artifact: %+v", artifact)
	}
	if artifact.SHA256 != "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824" {
		t.Fatalf("unexpected artifact digest: %s", artifact.SHA256)
	}
	if artifact.HostPath != destination {
		t.Fatalf("host path=%q", artifact.HostPath)
	}
	written, err := os.ReadFile(destination)
	if err != nil || string(written) != "hello" {
		t.Fatalf("destination=%q err=%v", written, err)
	}
	if err := os.WriteFile(destination, []byte("keep"), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := sandbox.Files().Export(context.Background(), "/workspace/artifact.bin", ArtifactTo(destination)); !errors.Is(err, ErrRuntime) {
		t.Fatalf("expected exclusive-create runtime error, got %v", err)
	}
	written, err = os.ReadFile(destination)
	if err != nil || string(written) != "keep" {
		t.Fatalf("existing destination changed: %q, %v", written, err)
	}

	requests := runtime.Requests()
	if requests[0]["user"] != "1000" || requests[1]["user"] != "1000" {
		t.Fatalf("artifact user was not forwarded: %#v", requests[:2])
	}
	if requests[1]["max_bytes"] != float64(5) {
		t.Fatalf("artifact max bytes was not forwarded: %#v", requests[1])
	}
}

func TestArtifactExportRejectsInvalidSourcesAndRacingResponses(t *testing.T) {
	t.Run("invalid limits do not reach runtime", func(t *testing.T) {
		runtime := &fakeRuntime{}
		sandbox := newSandbox(runtime, SandboxInfo{SandboxID: "box-1", Generation: 1, State: StateRunning, Isolation: IsolationMicroVM})
		for _, limit := range []uint64{0, MaxArtifactBytes + 1} {
			if _, err := sandbox.Files().Export(context.Background(), "/output", ArtifactMaxBytes(limit)); !errors.Is(err, ErrInvalidRequest) {
				t.Fatalf("limit=%d error=%v", limit, err)
			}
		}
		for _, request := range []struct {
			path    string
			options []ArtifactExportOption
		}{
			{path: "  "},
			{path: "/output", options: []ArtifactExportOption{ArtifactTo("  ")}},
		} {
			if _, err := sandbox.Files().Export(context.Background(), request.path, request.options...); !errors.Is(err, ErrInvalidRequest) {
				t.Fatalf("path=%q error=%v", request.path, err)
			}
		}
		if len(runtime.Requests()) != 0 {
			t.Fatal("invalid artifact limit reached runtime")
		}
	})

	tests := []struct {
		name       string
		entry      EntryInfo
		data       []byte
		declared   uint64
		maxBytes   uint64
		expected   error
		expectRead bool
	}{
		{name: "directory", entry: EntryInfo{Type: "directory"}, maxBytes: MaxArtifactBytes, expected: ErrInvalidRequest},
		{name: "oversized", entry: EntryInfo{Type: "file", Size: 6}, maxBytes: 5, expected: ErrInvalidRequest},
		{name: "malformed size", entry: EntryInfo{Type: "file", Size: 5}, data: []byte("hello"), declared: 6, maxBytes: MaxArtifactBytes, expected: ErrProtocol, expectRead: true},
		{name: "stat read race", entry: EntryInfo{Type: "file", Size: 5}, data: []byte("four"), declared: 4, maxBytes: MaxArtifactBytes, expected: ErrProtocol, expectRead: true},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			runtime := &fakeRuntime{handler: func(_ context.Context, request map[string]any) (any, error) {
				switch request["operation"] {
				case "filesystem_stat":
					return map[string]any{"entry": test.entry}, nil
				case "file_read":
					return map[string]any{"path": request["path"], "data_base64": base64.StdEncoding.EncodeToString(test.data), "size": test.declared}, nil
				default:
					return nil, errors.New("unexpected operation")
				}
			}}
			sandbox := newSandbox(runtime, SandboxInfo{SandboxID: "box-1", Generation: 1, State: StateRunning, Isolation: IsolationMicroVM})
			_, err := sandbox.Files().Export(context.Background(), "/output", ArtifactMaxBytes(test.maxBytes))
			if !errors.Is(err, test.expected) {
				t.Fatalf("expected %v, got %v", test.expected, err)
			}
			read := false
			for _, request := range runtime.Requests() {
				read = read || request["operation"] == "file_read"
			}
			if read != test.expectRead {
				t.Fatalf("file_read=%v, expected %v", read, test.expectRead)
			}
		})
	}
}

func TestCommandValidationDoesNotReachRuntime(t *testing.T) {
	runtime := &fakeRuntime{}
	sandbox := newSandbox(runtime, SandboxInfo{SandboxID: "box-1", Generation: 1, State: StateRunning, Isolation: IsolationMicroVM})
	if _, err := sandbox.Run(context.Background(), Argv()); !errors.Is(err, ErrInvalidRequest) {
		t.Fatalf("expected empty command validation, got %v", err)
	}
	if _, err := sandbox.Script("").Run(context.Background()); !errors.Is(err, ErrInvalidRequest) {
		t.Fatalf("expected empty script validation, got %v", err)
	}
	if _, err := sandbox.Files().List(context.Background(), "/", 0); !errors.Is(err, ErrInvalidRequest) {
		t.Fatalf("expected depth validation, got %v", err)
	}
	if len(runtime.Requests()) != 0 {
		t.Fatal("invalid terminal operations reached runtime")
	}
}

func TestEventStreamIsBackpressuredPausedAndGenerationFenced(t *testing.T) {
	var generation atomic.Uint64
	generation.Store(1)
	runtime := &fakeRuntime{handler: func(_ context.Context, request map[string]any) (any, error) {
		switch request["operation"] {
		case "sandbox_events":
			requestGeneration := uint64(request["generation"].(float64))
			if requestGeneration != generation.Load() {
				return nil, sdkError("sandbox_events", CodeConflict, "stale generation", nil)
			}
			after := uint64(request["after_sequence"].(float64))
			limit := int(request["limit"].(float64))
			sequences := []uint64{2, 5, 9}
			events := make([]ExecutionRuntimeEvent, 0, limit)
			for _, sequence := range sequences {
				if sequence <= after || len(events) == limit {
					continue
				}
				events = append(events, ExecutionRuntimeEvent{
					Sequence:        sequence,
					TimestampUnixNS: 1_700_000_000_000_000_000 + sequence,
					Kind:            EventResourcesUpdated,
					Attributes:      map[string]string{},
				})
			}
			next := after
			if len(events) != 0 {
				next = events[len(events)-1].Sequence
			}
			return ExecutionEventBatch{
				ExecutionID:  "box-1",
				Generation:   requestGeneration,
				Events:       events,
				NextSequence: next,
			}, nil
		case "sandbox_pause":
			return SandboxInfo{SandboxID: "box-1", Generation: generation.Load(), State: StatePaused, Isolation: IsolationSandbox}, nil
		case "sandbox_resume":
			return SandboxInfo{SandboxID: "box-1", Generation: generation.Load(), State: StateRunning, Isolation: IsolationSandbox}, nil
		case "sandbox_restart":
			next := generation.Add(1)
			return SandboxInfo{SandboxID: "box-1", Generation: next, State: StateRunning, Isolation: IsolationSandbox}, nil
		default:
			return nil, errors.New("unexpected operation")
		}
	}}
	sandbox := newSandbox(runtime, SandboxInfo{SandboxID: "box-1", Generation: 1, State: StateRunning, Isolation: IsolationSandbox})

	stream, err := sandbox.StreamEvents(ExecutionEventStreamOptions{
		BatchLimit:  2,
		WaitTimeout: time.Millisecond,
	})
	if err != nil {
		t.Fatal(err)
	}
	for index, expected := range []uint64{2, 5, 9} {
		event, nextErr := stream.Next(context.Background())
		if nextErr != nil || event.Sequence != expected {
			t.Fatalf("event %d: %+v, %v", index, event, nextErr)
		}
		if stream.Cursor() != expected {
			t.Fatalf("cursor=%d, expected %d", stream.Cursor(), expected)
		}
	}
	polls := make([]map[string]any, 0, 2)
	for _, request := range runtime.Requests() {
		if request["operation"] == "sandbox_events" {
			polls = append(polls, request)
		}
	}
	if len(polls) != 2 || polls[0]["after_sequence"] != float64(0) || polls[1]["after_sequence"] != float64(5) {
		t.Fatalf("unexpected event polls: %#v", polls)
	}

	if err := sandbox.Pause(context.Background(), true); err != nil {
		t.Fatal(err)
	}
	wait := uint64(1)
	paused, err := sandbox.Events(context.Background(), ExecutionEventsRequest{AfterSequence: 9, Limit: 1, WaitTimeoutMS: &wait})
	if err != nil || len(paused.Events) != 0 {
		t.Fatalf("paused events=%+v err=%v", paused, err)
	}
	if err := sandbox.Resume(context.Background()); err != nil {
		t.Fatal(err)
	}

	fenced, err := sandbox.StreamEvents(ExecutionEventStreamOptions{BatchLimit: 1, WaitTimeout: time.Millisecond})
	if err != nil {
		t.Fatal(err)
	}
	if event, nextErr := fenced.Next(context.Background()); nextErr != nil || event.Sequence != 2 {
		t.Fatalf("first fenced event=%+v err=%v", event, nextErr)
	}
	if err := sandbox.Restart(context.Background(), RestartOperationID("go-event-stream-restart")); err != nil {
		t.Fatal(err)
	}
	if _, err := fenced.Next(context.Background()); !errors.Is(err, ErrConflict) {
		t.Fatalf("expected generation conflict, got %v", err)
	}
	if _, err := fenced.Next(context.Background()); !errors.Is(err, io.EOF) {
		t.Fatalf("expected terminal EOF, got %v", err)
	}
}

func TestEventStreamCloseCancelsActivePoll(t *testing.T) {
	started := make(chan struct{})
	var once sync.Once
	runtime := &fakeRuntime{handler: func(ctx context.Context, request map[string]any) (any, error) {
		if request["operation"] != "sandbox_events" {
			return nil, errors.New("unexpected operation")
		}
		once.Do(func() { close(started) })
		<-ctx.Done()
		return nil, ctx.Err()
	}}
	sandbox := newSandbox(runtime, SandboxInfo{SandboxID: "box-1", Generation: 1, State: StateRunning, Isolation: IsolationSandbox})
	stream, err := sandbox.StreamEvents(ExecutionEventStreamOptions{WaitTimeout: time.Second})
	if err != nil {
		t.Fatal(err)
	}
	done := make(chan error, 1)
	go func() {
		_, nextErr := stream.Next(context.Background())
		done <- nextErr
	}()
	<-started
	if err := stream.Close(); err != nil {
		t.Fatal(err)
	}
	select {
	case err := <-done:
		if !errors.Is(err, io.EOF) {
			t.Fatalf("expected EOF after close, got %v", err)
		}
	case <-time.After(time.Second):
		t.Fatal("event stream close did not cancel its active poll")
	}
	if _, err := stream.Next(context.Background()); !errors.Is(err, io.EOF) {
		t.Fatalf("expected closed stream EOF, got %v", err)
	}
}

func TestEventStreamOptionsValidateBeforeRuntimeAccess(t *testing.T) {
	runtime := &fakeRuntime{}
	sandbox := newSandbox(runtime, SandboxInfo{SandboxID: "box-1", Generation: 1, State: StateRunning, Isolation: IsolationSandbox})
	for _, options := range []ExecutionEventStreamOptions{
		{BatchLimit: MaxExecutionEventBatchItems + 1},
		{WaitTimeout: -time.Millisecond},
		{WaitTimeout: time.Microsecond},
	} {
		if _, err := sandbox.StreamEvents(options); !errors.Is(err, ErrInvalidRequest) {
			t.Fatalf("expected invalid options error, got %v", err)
		}
	}
	if len(runtime.Requests()) != 0 {
		t.Fatal("invalid event stream options reached runtime")
	}
}

func assertGeneration(t *testing.T, request map[string]any, expected float64) {
	t.Helper()
	if request["generation"] != expected {
		t.Fatalf("expected generation %.0f, got %#v", expected, request["generation"])
	}
}

func stringValue(value any) string {
	text, _ := value.(string)
	return text
}
