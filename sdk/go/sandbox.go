package box

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"strings"
	"sync"
	"time"
)

const defaultCloseTimeout = 30 * time.Second

// Sandbox is a concurrency-safe handle to one local execution generation.
// Command and filesystem calls can run concurrently. Lifecycle transitions are
// serialized so no call is issued with a stale generation.
type Sandbox struct {
	runtime Runtime
	id      string

	mu         sync.RWMutex
	generation uint64
	state      SandboxState
	isolation  Isolation
}

func newSandbox(runtime Runtime, info SandboxInfo) *Sandbox {
	return &Sandbox{
		runtime:    runtime,
		id:         info.SandboxID,
		generation: info.Generation,
		state:      info.State,
		isolation:  info.Isolation,
	}
}

func (client *Client) ConnectSandbox(ctx context.Context, sandboxID string) (*Sandbox, error) {
	const op = "sandbox_inspect"
	if strings.TrimSpace(sandboxID) == "" {
		return nil, invalid(op, "sandbox ID cannot be empty")
	}
	var info SandboxInfo
	if err := client.request(ctx, op, map[string]any{"sandbox_id": sandboxID}, &info); err != nil {
		return nil, err
	}
	if err := validateSandboxInfo(op, sandboxID, "", info); err != nil {
		return nil, err
	}
	return newSandbox(client.runtime, info), nil
}

func (sandbox *Sandbox) ID() string {
	if sandbox == nil {
		return ""
	}
	return sandbox.id
}

func (sandbox *Sandbox) Generation() uint64 {
	if sandbox == nil {
		return 0
	}
	sandbox.mu.RLock()
	defer sandbox.mu.RUnlock()
	return sandbox.generation
}

func (sandbox *Sandbox) State() SandboxState {
	if sandbox == nil {
		return ""
	}
	sandbox.mu.RLock()
	defer sandbox.mu.RUnlock()
	return sandbox.state
}

func (sandbox *Sandbox) Isolation() Isolation {
	if sandbox == nil {
		return ""
	}
	sandbox.mu.RLock()
	defer sandbox.mu.RUnlock()
	return sandbox.isolation
}

func (sandbox *Sandbox) Inspect(ctx context.Context) (SandboxInfo, error) {
	const op = "sandbox_inspect"
	if sandbox == nil || runtimeIsNil(sandbox.runtime) {
		return SandboxInfo{}, invalid(op, "sandbox is not initialized")
	}
	sandbox.mu.Lock()
	defer sandbox.mu.Unlock()
	var info SandboxInfo
	if err := sandbox.requestLocked(ctx, op, map[string]any{"sandbox_id": sandbox.id}, &info); err != nil {
		return SandboxInfo{}, err
	}
	if err := validateSandboxInfo(op, sandbox.id, sandbox.isolation, info); err != nil {
		return SandboxInfo{}, err
	}
	sandbox.updateLocked(info, sandbox.state)
	return info, nil
}

func (sandbox *Sandbox) IsRunning(ctx context.Context) (bool, error) {
	info, err := sandbox.Inspect(ctx)
	if err != nil {
		if errors.Is(err, ErrNotFound) {
			return false, nil
		}
		return false, err
	}
	return info.State == StateRunning, nil
}

func (sandbox *Sandbox) Stop(ctx context.Context) error {
	const op = "sandbox_stop"
	if sandbox == nil || runtimeIsNil(sandbox.runtime) {
		return invalid(op, "sandbox is not initialized")
	}
	sandbox.mu.Lock()
	defer sandbox.mu.Unlock()
	if sandbox.state == StateStopped || sandbox.terminalLocked() {
		return nil
	}
	var info SandboxInfo
	if err := sandbox.lifecycleRequestLocked(ctx, op, nil, &info); err != nil {
		return err
	}
	if err := validateSandboxInfo(op, sandbox.id, sandbox.isolation, info); err != nil {
		return err
	}
	sandbox.updateLocked(info, StateStopped)
	return nil
}

type RestartOption interface{ applyRestart(*restartConfig) }
type restartOptionFunc func(*restartConfig)

func (option restartOptionFunc) applyRestart(config *restartConfig) { option(config) }

type restartConfig struct {
	operationID string
	stopTimeout *time.Duration
}

func RestartOperationID(operationID string) RestartOption {
	return restartOptionFunc(func(config *restartConfig) { config.operationID = operationID })
}

func RestartStopTimeout(timeout time.Duration) RestartOption {
	return restartOptionFunc(func(config *restartConfig) { config.stopTimeout = &timeout })
}

func (sandbox *Sandbox) Restart(ctx context.Context, options ...RestartOption) error {
	const op = "sandbox_restart"
	if sandbox == nil || runtimeIsNil(sandbox.runtime) {
		return invalid(op, "sandbox is not initialized")
	}
	config := restartConfig{}
	for _, option := range options {
		if option != nil {
			option.applyRestart(&config)
		}
	}
	if config.operationID != "" && strings.TrimSpace(config.operationID) == "" {
		return invalid(op, "restart operation ID cannot be blank")
	}
	if config.stopTimeout != nil && *config.stopTimeout < 0 {
		return invalid(op, "restart stop timeout cannot be negative")
	}
	if config.operationID == "" {
		operationID, err := randomOperationID()
		if err != nil {
			return sdkError(op, CodeRuntime, "cannot generate restart operation ID", err)
		}
		config.operationID = operationID
	}
	sandbox.mu.Lock()
	defer sandbox.mu.Unlock()
	if sandbox.terminalLocked() {
		return invalid(op, "sandbox has been removed")
	}
	extra := map[string]any{"operation_id": config.operationID}
	if config.stopTimeout != nil {
		extra["stop_timeout_seconds"] = durationSeconds(*config.stopTimeout)
	}
	var info SandboxInfo
	if err := sandbox.lifecycleRequestLocked(ctx, op, extra, &info); err != nil {
		return err
	}
	if err := validateSandboxInfo(op, sandbox.id, sandbox.isolation, info); err != nil {
		return err
	}
	sandbox.updateLocked(info, StateRunning)
	return nil
}

func (sandbox *Sandbox) Remove(ctx context.Context) error {
	const op = "sandbox_remove"
	if sandbox == nil || runtimeIsNil(sandbox.runtime) {
		return invalid(op, "sandbox is not initialized")
	}
	sandbox.mu.Lock()
	defer sandbox.mu.Unlock()
	if sandbox.terminalLocked() {
		return nil
	}
	var info SandboxInfo
	if err := sandbox.lifecycleRequestLocked(ctx, op, nil, &info); err != nil {
		return err
	}
	if err := validateSandboxInfo(op, sandbox.id, sandbox.isolation, info); err != nil {
		return err
	}
	sandbox.updateLocked(info, StateRemoved)
	return nil
}

func (sandbox *Sandbox) Kill(ctx context.Context) error {
	const op = "sandbox_kill"
	if sandbox == nil || runtimeIsNil(sandbox.runtime) {
		return invalid(op, "sandbox is not initialized")
	}
	sandbox.mu.Lock()
	defer sandbox.mu.Unlock()
	if sandbox.terminalLocked() {
		return nil
	}
	var info SandboxInfo
	if err := sandbox.lifecycleRequestLocked(ctx, op, nil, &info); err != nil {
		return err
	}
	if err := validateSandboxInfo(op, sandbox.id, sandbox.isolation, info); err != nil {
		return err
	}
	sandbox.updateLocked(info, StateKilled)
	sandbox.state = StateKilled
	return nil
}

func (sandbox *Sandbox) Pause(ctx context.Context, keepMemory bool) error {
	const op = "sandbox_pause"
	if sandbox == nil || runtimeIsNil(sandbox.runtime) {
		return invalid(op, "sandbox is not initialized")
	}
	sandbox.mu.Lock()
	defer sandbox.mu.Unlock()
	if sandbox.state != StateRunning {
		return invalid(op, "only a running sandbox can be paused")
	}
	var info SandboxInfo
	if err := sandbox.lifecycleRequestLocked(ctx, op, map[string]any{"keep_memory": keepMemory}, &info); err != nil {
		return err
	}
	if err := validateSandboxInfo(op, sandbox.id, sandbox.isolation, info); err != nil {
		return err
	}
	sandbox.updateLocked(info, StatePaused)
	return nil
}

func (sandbox *Sandbox) Resume(ctx context.Context) error {
	const op = "sandbox_resume"
	if sandbox == nil || runtimeIsNil(sandbox.runtime) {
		return invalid(op, "sandbox is not initialized")
	}
	sandbox.mu.Lock()
	defer sandbox.mu.Unlock()
	if sandbox.state != StatePaused {
		return invalid(op, "only a paused sandbox can be resumed")
	}
	var info SandboxInfo
	if err := sandbox.lifecycleRequestLocked(ctx, op, nil, &info); err != nil {
		return err
	}
	if err := validateSandboxInfo(op, sandbox.id, sandbox.isolation, info); err != nil {
		return err
	}
	sandbox.updateLocked(info, StateRunning)
	return nil
}

func (sandbox *Sandbox) Logs(ctx context.Context, tail int) ([]SandboxLogEntry, error) {
	const op = "sandbox_logs"
	if tail < 1 || tail > 10_000 {
		return nil, invalid(op, "log tail must be between 1 and 10000")
	}
	var result struct {
		Logs []SandboxLogEntry `json:"logs"`
	}
	err := sandbox.readRequest(ctx, op, map[string]any{"tail": tail}, &result, false)
	return result.Logs, err
}

func (sandbox *Sandbox) Stats(ctx context.Context) (*SandboxStats, error) {
	const op = "sandbox_stats"
	var result struct {
		Stats *SandboxStats `json:"stats"`
	}
	if err := sandbox.readRequest(ctx, op, nil, &result, false); err != nil {
		return nil, err
	}
	return result.Stats, nil
}

func (sandbox *Sandbox) CreateFilesystemSnapshot(
	ctx context.Context,
	snapshotID string,
) (FilesystemSnapshotInfo, error) {
	const op = "sandbox_snapshot_create"
	if strings.TrimSpace(snapshotID) == "" {
		return FilesystemSnapshotInfo{}, invalid(op, "snapshot ID cannot be empty")
	}
	var result FilesystemSnapshotInfo
	err := sandbox.readRequest(ctx, op, map[string]any{"snapshot_id": snapshotID}, &result, false)
	return result, err
}

// Close performs bounded, idempotent cleanup. A failed cleanup is retryable.
func (sandbox *Sandbox) Close(ctx context.Context) error {
	if ctx == nil {
		return invalid("sandbox_close", "context cannot be nil")
	}
	closeContext, cancel := context.WithTimeout(ctx, defaultCloseTimeout)
	defer cancel()
	return sandbox.Kill(closeContext)
}

func (sandbox *Sandbox) readRequest(
	ctx context.Context,
	operation string,
	extra map[string]any,
	result any,
	requireRunning bool,
) error {
	if sandbox == nil || runtimeIsNil(sandbox.runtime) {
		return invalid(operation, "sandbox is not initialized")
	}
	sandbox.mu.RLock()
	defer sandbox.mu.RUnlock()
	if sandbox.terminalLocked() {
		return invalid(operation, "sandbox has been removed")
	}
	if requireRunning && sandbox.state != StateRunning {
		return invalid(operation, "sandbox is not running")
	}
	fields := map[string]any{
		"sandbox_id": sandbox.id,
		"generation": sandbox.generation,
	}
	for key, value := range extra {
		fields[key] = value
	}
	return sandbox.requestLocked(ctx, operation, fields, result)
}

func (sandbox *Sandbox) lifecycleRequestLocked(
	ctx context.Context,
	operation string,
	extra map[string]any,
	result any,
) error {
	fields := map[string]any{
		"sandbox_id": sandbox.id,
		"generation": sandbox.generation,
	}
	for key, value := range extra {
		fields[key] = value
	}
	return sandbox.requestLocked(ctx, operation, fields, result)
}

func (sandbox *Sandbox) requestLocked(
	ctx context.Context,
	operation string,
	fields map[string]any,
	result any,
) error {
	if ctx == nil {
		return invalid(operation, "context cannot be nil")
	}
	request := make(map[string]any, len(fields)+1)
	request["operation"] = operation
	for key, value := range fields {
		request[key] = value
	}
	return sandbox.runtime.Request(ctx, request, result)
}

func (sandbox *Sandbox) terminalLocked() bool {
	return sandbox.state == StateKilled || sandbox.state == StateRemoved
}

func (sandbox *Sandbox) updateLocked(info SandboxInfo, fallback SandboxState) {
	if info.Generation != 0 {
		sandbox.generation = info.Generation
	}
	if info.State != "" {
		sandbox.state = info.State
	} else {
		sandbox.state = fallback
	}
	if info.Isolation != "" {
		sandbox.isolation = info.Isolation
	}
}

func validateSandboxInfo(
	operation string,
	expectedID string,
	expectedIsolation Isolation,
	info SandboxInfo,
) error {
	if strings.TrimSpace(info.SandboxID) == "" {
		return sdkError(operation, CodeProtocol, "bridge result is missing sandbox_id", nil)
	}
	if expectedID != "" && info.SandboxID != expectedID {
		return sdkError(operation, CodeProtocol, "bridge result returned a different sandbox_id", nil)
	}
	if info.Generation == 0 {
		return sdkError(operation, CodeProtocol, "bridge result has an invalid generation", nil)
	}
	switch info.State {
	case StateCreated, StateCreating, StateRunning, StatePaused, StateStopped, StateFailed, StateKilled, StateRemoved:
	default:
		return sdkError(operation, CodeProtocol, "bridge result has an invalid sandbox state", nil)
	}
	if info.Isolation != IsolationMicroVM && info.Isolation != IsolationSandbox {
		return sdkError(operation, CodeProtocol, "bridge result has an invalid isolation", nil)
	}
	if expectedIsolation != "" && info.Isolation != expectedIsolation {
		return sdkError(operation, CodeProtocol, "bridge result changed sandbox isolation", nil)
	}
	return nil
}

func randomOperationID() (string, error) {
	var value [16]byte
	if _, err := rand.Read(value[:]); err != nil {
		return "", err
	}
	return "sdk-restart-" + hex.EncodeToString(value[:]), nil
}
