package box

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/A3S-Lab/Box/sdk/go/v3/internal/bridge"
)

func TestMain(m *testing.M) {
	if len(os.Args) > 1 && os.Args[1] == "sdk-bridge" {
		runBridgeHelper()
		os.Exit(0)
	}
	os.Exit(m.Run())
}

func runBridgeHelper() {
	mode := os.Getenv("A3S_BOX_GO_TEST_HELPER_MODE")
	payload, _ := io.ReadAll(os.Stdin)
	switch mode {
	case "malformed":
		fmt.Print("not-json")
	case "trailing":
		fmt.Printf(`{"protocol_version":%d,"ok":true,"result":{}} {}`, bridge.ProtocolVersion)
	case "bad_version":
		fmt.Print(`{"protocol_version":99,"ok":true,"result":{}}`)
	case "non_object":
		fmt.Printf(`{"protocol_version":%d,"ok":true,"result":[]}`, bridge.ProtocolVersion)
	case "exit":
		fmt.Fprint(os.Stderr, "helper process failed")
		os.Exit(7)
	case "wait":
		for {
			time.Sleep(time.Hour)
		}
	case "credentials":
		if len(os.Args) != 2 || os.Args[1] != "sdk-bridge" || strings.Contains(strings.Join(os.Args, " "), "super-secret") {
			fmt.Fprint(os.Stderr, "credentials leaked through argv")
			os.Exit(8)
		}
		if !strings.Contains(string(payload), `"password":"super-secret"`) {
			fmt.Fprint(os.Stderr, "credential missing from stdin")
			os.Exit(9)
		}
		writeHelperEnvelope(true, map[string]any{"accepted": true}, "", "")
	case "error":
		writeHelperEnvelope(false, nil, os.Getenv("A3S_BOX_GO_TEST_ERROR_CODE"), "bridge rejected request")
	default:
		writeHelperEnvelope(true, map[string]any{"value": "ok"}, "", "")
	}
}

func writeHelperEnvelope(ok bool, result any, code, message string) {
	envelope := map[string]any{"protocol_version": bridge.ProtocolVersion, "ok": ok}
	if ok {
		envelope["result"] = result
	} else {
		envelope["error"] = map[string]string{"code": code, "message": message}
	}
	_ = json.NewEncoder(os.Stdout).Encode(envelope)
}

func TestLocalRuntimeRoundTripAndEnvironmentResolution(t *testing.T) {
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	t.Setenv("A3S_BOX_BINARY", executable)
	t.Setenv("A3S_BOX_GO_TEST_HELPER_MODE", "success")
	runtime := NewLocalRuntime(WithBridgeTimeout(10 * time.Second))
	var result struct {
		Value string `json:"value"`
	}
	if err := runtime.Request(context.Background(), map[string]any{"operation": "runtime_diagnostics"}, &result); err != nil {
		t.Fatal(err)
	}
	if result.Value != "ok" {
		t.Fatalf("unexpected result: %+v", result)
	}
}

func TestLocalRuntimeSendsCredentialsOnlyThroughStdin(t *testing.T) {
	t.Setenv("A3S_BOX_GO_TEST_HELPER_MODE", "credentials")
	runtime := helperRuntime(t, 10*time.Second)
	request := map[string]any{
		"operation": "image_pull",
		"credentials": map[string]string{
			"username": "ci",
			"password": "super-secret",
		},
	}
	var result struct {
		Accepted bool `json:"accepted"`
	}
	if err := runtime.Request(context.Background(), request, &result); err != nil {
		t.Fatal(err)
	}
	if !result.Accepted {
		t.Fatal("helper did not accept credentials")
	}
}

func TestLocalRuntimeMapsStableBridgeErrors(t *testing.T) {
	t.Setenv("A3S_BOX_GO_TEST_HELPER_MODE", "error")
	tests := []struct {
		code   string
		target error
	}{
		{"invalid_request", ErrInvalidRequest},
		{"not_found", ErrNotFound},
		{"conflict", ErrConflict},
		{"unavailable", ErrUnavailable},
		{"runtime_error", ErrRuntime},
	}
	for _, test := range tests {
		t.Run(test.code, func(t *testing.T) {
			t.Setenv("A3S_BOX_GO_TEST_ERROR_CODE", test.code)
			err := helperRuntime(t, 10*time.Second).Request(
				context.Background(),
				map[string]any{"operation": "sandbox_create"},
				&struct{}{},
			)
			if !errors.Is(err, test.target) {
				t.Fatalf("expected %v, got %v", test.target, err)
			}
			var sdkErr *Error
			if !errors.As(err, &sdkErr) || sdkErr.Op != "sandbox_create" {
				t.Fatalf("missing typed operation context: %#v", err)
			}
		})
	}
}

func TestLocalRuntimeRejectsMalformedProtocol(t *testing.T) {
	for _, mode := range []string{"malformed", "trailing", "bad_version", "non_object"} {
		t.Run(mode, func(t *testing.T) {
			t.Setenv("A3S_BOX_GO_TEST_HELPER_MODE", mode)
			err := helperRuntime(t, 10*time.Second).Request(
				context.Background(),
				map[string]any{"operation": "image_list"},
				&struct{}{},
			)
			if !errors.Is(err, ErrProtocol) {
				t.Fatalf("expected protocol error, got %v", err)
			}
		})
	}
}

func TestLocalRuntimeReportsMissingBinary(t *testing.T) {
	runtime := NewLocalRuntime(WithBinaryPath(t.TempDir() + "/missing"))
	err := runtime.Request(context.Background(), map[string]any{"operation": "image_list"}, &struct{}{})
	if !errors.Is(err, ErrBinaryNotFound) || !errors.Is(err, ErrNotInstalled) {
		t.Fatalf("expected binary-not-found error, got %v", err)
	}
	var sdkErr *Error
	if !errors.As(err, &sdkErr) || sdkErr.Code != CodeBinaryNotFound {
		t.Fatalf("expected stable %q code, got %v", CodeBinaryNotFound, err)
	}
}

func TestLocalRuntimePreservesCancellationAndDeadline(t *testing.T) {
	t.Run("caller cancellation", func(t *testing.T) {
		t.Setenv("A3S_BOX_GO_TEST_HELPER_MODE", "wait")
		ctx, cancel := context.WithCancel(context.Background())
		cancel()
		err := helperRuntime(t, 10*time.Second).Request(ctx, map[string]any{"operation": "command_run"}, &struct{}{})
		if !errors.Is(err, context.Canceled) || !errors.Is(err, ErrCanceled) {
			t.Fatalf("expected preserved cancellation, got %v", err)
		}
	})

	t.Run("caller deadline", func(t *testing.T) {
		t.Setenv("A3S_BOX_GO_TEST_HELPER_MODE", "wait")
		ctx, cancel := context.WithTimeout(context.Background(), 30*time.Millisecond)
		defer cancel()
		err := helperRuntime(t, 10*time.Second).Request(ctx, map[string]any{"operation": "command_run"}, &struct{}{})
		if !errors.Is(err, context.DeadlineExceeded) || !errors.Is(err, ErrDeadlineExceeded) {
			t.Fatalf("expected preserved deadline, got %v", err)
		}
	})

	t.Run("runtime deadline", func(t *testing.T) {
		t.Setenv("A3S_BOX_GO_TEST_HELPER_MODE", "wait")
		err := helperRuntime(t, 30*time.Millisecond).Request(
			context.Background(),
			map[string]any{"operation": "command_run"},
			&struct{}{},
		)
		if !errors.Is(err, context.DeadlineExceeded) || !errors.Is(err, ErrBridgeTimeout) {
			t.Fatalf("expected bridge timeout, got %v", err)
		}
	})
}

func TestLocalRuntimeReportsProcessFailureWithoutParsingHumanOutput(t *testing.T) {
	t.Setenv("A3S_BOX_GO_TEST_HELPER_MODE", "exit")
	err := helperRuntime(t, 10*time.Second).Request(
		context.Background(),
		map[string]any{"operation": "image_list"},
		&struct{}{},
	)
	if !errors.Is(err, ErrRuntime) || !strings.Contains(err.Error(), "helper process failed") {
		t.Fatalf("unexpected process failure: %v", err)
	}
}

func helperRuntime(t *testing.T, timeout time.Duration) *LocalRuntime {
	t.Helper()
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	return NewLocalRuntime(WithBinaryPath(executable), WithBridgeTimeout(timeout))
}
