package box

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"

	"github.com/A3S-Lab/Box/sdk/go/v3/internal/bridge"
)

const defaultBridgeTimeout = 10 * time.Minute
const bridgeProcessWaitDelay = 2 * time.Second

// Runtime is the typed extension point used by Client. Implementations must be
// safe for concurrent calls. The built-in LocalRuntime invokes the installed
// a3s-box binary and does not require an endpoint or API key.
type Runtime interface {
	Request(ctx context.Context, request any, result any) error
}

// LocalRuntime invokes one structured sdk-bridge process per request.
type LocalRuntime struct {
	binaryPath string
	timeout    time.Duration
}

type LocalRuntimeOption interface {
	applyLocalRuntime(*LocalRuntime)
}

type localRuntimeOptionFunc func(*LocalRuntime)

func (option localRuntimeOptionFunc) applyLocalRuntime(runtime *LocalRuntime) {
	option(runtime)
}

// WithBinaryPath selects an a3s-box binary. When omitted, LocalRuntime reads
// A3S_BOX_BINARY and then falls back to a3s-box on PATH.
func WithBinaryPath(path string) LocalRuntimeOption {
	return localRuntimeOptionFunc(func(runtime *LocalRuntime) {
		runtime.binaryPath = path
	})
}

// WithBridgeTimeout bounds a single bridge process. Callers can always set a
// shorter deadline on their context.
func WithBridgeTimeout(timeout time.Duration) LocalRuntimeOption {
	return localRuntimeOptionFunc(func(runtime *LocalRuntime) {
		runtime.timeout = timeout
	})
}

func NewLocalRuntime(options ...LocalRuntimeOption) *LocalRuntime {
	runtime := &LocalRuntime{timeout: defaultBridgeTimeout}
	for _, option := range options {
		if option != nil {
			option.applyLocalRuntime(runtime)
		}
	}
	return runtime
}

func (runtime *LocalRuntime) Request(ctx context.Context, request any, result any) error {
	const op = "sdk-bridge"
	if ctx == nil {
		return invalid(op, "context cannot be nil")
	}
	if runtime == nil {
		return invalid(op, "local runtime cannot be nil")
	}
	if runtime.timeout <= 0 {
		return invalid(op, "bridge timeout must be greater than zero")
	}

	payload, err := json.Marshal(request)
	if err != nil {
		return sdkError(op, CodeInvalidRequest, "cannot encode bridge request", err)
	}
	operation := requestOperation(payload)
	binary, err := runtime.resolveBinary()
	if err != nil {
		return sdkError(operation, CodeNotInstalled, err.Error(), err)
	}

	requestContext, cancel := context.WithTimeout(ctx, runtime.timeout)
	defer cancel()

	command := exec.CommandContext(requestContext, binary, "sdk-bridge")
	command.WaitDelay = bridgeProcessWaitDelay
	command.Stdin = bytes.NewReader(payload)
	var stdout bytes.Buffer
	var stderr bytes.Buffer
	command.Stdout = &stdout
	command.Stderr = &stderr
	runErr := command.Run()
	if requestContext.Err() != nil {
		return contextError(operation, requestContext.Err())
	}

	decodeErr := decodeBridgeResponse(operation, stdout.Bytes(), result)
	if decodeErr != nil {
		if errors.Is(decodeErr, ErrProtocol) && runErr != nil {
			detail := strings.TrimSpace(stderr.String())
			if detail == "" {
				detail = runErr.Error()
			}
			return sdkError(operation, CodeRuntime, "local bridge process failed: "+truncate(detail, 4096), runErr)
		}
		return decodeErr
	}
	if runErr != nil {
		detail := strings.TrimSpace(stderr.String())
		if detail == "" {
			detail = runErr.Error()
		}
		return sdkError(operation, CodeRuntime, "local bridge process failed: "+truncate(detail, 4096), runErr)
	}
	return nil
}

func (runtime *LocalRuntime) resolveBinary() (string, error) {
	candidate := strings.TrimSpace(runtime.binaryPath)
	if candidate == "" {
		candidate = strings.TrimSpace(os.Getenv("A3S_BOX_BINARY"))
	}
	if candidate == "" {
		candidate = "a3s-box"
	}
	if strings.ContainsRune(candidate, os.PathSeparator) || filepath.IsAbs(candidate) {
		info, err := os.Stat(candidate)
		if err != nil {
			return "", fmt.Errorf("A3S Box binary %q is not installed: %w", candidate, err)
		}
		if info.IsDir() {
			return "", fmt.Errorf("A3S Box binary %q is a directory", candidate)
		}
		return candidate, nil
	}
	resolved, err := exec.LookPath(candidate)
	if err != nil {
		return "", fmt.Errorf("A3S Box binary %q is not installed: %w", candidate, err)
	}
	return resolved, nil
}

func decodeBridgeResponse(operation string, output []byte, result any) error {
	decoder := json.NewDecoder(bytes.NewReader(output))
	var envelope bridge.Envelope
	if err := decoder.Decode(&envelope); err != nil {
		return sdkError(operation, CodeProtocol, "invalid response from the local bridge", err)
	}
	var trailing any
	if err := decoder.Decode(&trailing); !errors.Is(err, io.EOF) {
		if err == nil {
			err = errors.New("multiple JSON values")
		}
		return sdkError(operation, CodeProtocol, "invalid trailing data from the local bridge", err)
	}
	if envelope.ProtocolVersion != bridge.ProtocolVersion {
		return sdkError(
			operation,
			CodeProtocol,
			fmt.Sprintf("unsupported bridge protocol version %d", envelope.ProtocolVersion),
			nil,
		)
	}
	if !envelope.OK {
		if envelope.Error == nil {
			return sdkError(operation, CodeRuntime, "local bridge request failed", nil)
		}
		code := ErrorCode(envelope.Error.Code)
		if code == "" {
			code = CodeRuntime
		}
		message := envelope.Error.Message
		if message == "" {
			message = "local bridge request failed"
		}
		return sdkError(operation, code, message, nil)
	}
	trimmed := bytes.TrimSpace(envelope.Result)
	if len(trimmed) == 0 || trimmed[0] != '{' {
		return sdkError(operation, CodeProtocol, "local bridge response is missing an object result", nil)
	}
	if result == nil {
		return nil
	}
	if err := json.Unmarshal(trimmed, result); err != nil {
		return sdkError(operation, CodeProtocol, "cannot decode local bridge result", err)
	}
	return nil
}

func requestOperation(payload []byte) string {
	var header struct {
		Operation string `json:"operation"`
	}
	if json.Unmarshal(payload, &header) == nil && header.Operation != "" {
		return header.Operation
	}
	return "sdk-bridge"
}

func truncate(value string, limit int) string {
	if len(value) <= limit {
		return value
	}
	return value[:limit] + "..."
}
