package box

import (
	"context"
	"encoding/base64"
	"errors"
	"reflect"
	"strings"
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
			return map[string]any{"data_base64": base64.StdEncoding.EncodeToString(binaryOutput), "size": 3}, nil
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
