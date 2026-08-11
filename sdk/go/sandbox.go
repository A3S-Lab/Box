package box

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode"
)

const (
	defaultCloseTimeout             = 30 * time.Second
	DefaultExecutionEventBatchLimit = uint32(256)
	MaxExecutionEventBatchItems     = uint32(4_096)
)

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
		operationID, err := randomOperationID("sdk-restart-")
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

func (sandbox *Sandbox) Processes(ctx context.Context) (ExecutionProcessInventory, error) {
	const op = "sandbox_processes"
	var result ExecutionProcessInventory
	generation, err := sandbox.executionReadRequest(ctx, op, nil, &result)
	if err != nil {
		return ExecutionProcessInventory{}, err
	}
	if err := validateExecutionIdentity(op, sandbox.id, generation, result.ExecutionID, result.Generation); err != nil {
		return ExecutionProcessInventory{}, err
	}
	seen := make(map[string]struct{}, len(result.Processes))
	for _, process := range result.Processes {
		if strings.TrimSpace(process.ProcessID) == "" {
			return ExecutionProcessInventory{}, sdkError(op, CodeProtocol, "runtime process inventory contains an empty process ID", nil)
		}
		if process.PID != nil && *process.PID == 0 {
			return ExecutionProcessInventory{}, sdkError(op, CodeProtocol, "runtime process inventory contains PID zero", nil)
		}
		if _, exists := seen[process.ProcessID]; exists {
			return ExecutionProcessInventory{}, sdkError(op, CodeProtocol, "runtime process inventory contains a duplicate process ID", nil)
		}
		seen[process.ProcessID] = struct{}{}
	}
	return result, nil
}

func (sandbox *Sandbox) RuntimeStats(ctx context.Context) (ExecutionStats, error) {
	const op = "sandbox_runtime_stats"
	var result ExecutionStats
	generation, err := sandbox.executionReadRequest(ctx, op, nil, &result)
	if err != nil {
		return ExecutionStats{}, err
	}
	if err := validateExecutionIdentity(op, sandbox.id, generation, result.ExecutionID, result.Generation); err != nil {
		return ExecutionStats{}, err
	}
	if result.TimestampUnixNS == 0 {
		return ExecutionStats{}, sdkError(op, CodeProtocol, "runtime stats timestamp must be positive", nil)
	}
	if result.CPU.UserNS > result.CPU.UsageNS || result.CPU.SystemNS > result.CPU.UsageNS-result.CPU.UserNS {
		return ExecutionStats{}, sdkError(op, CodeProtocol, "runtime CPU counters exceed total usage", nil)
	}
	if result.Memory.PeakBytes != nil && *result.Memory.PeakBytes < result.Memory.UsageBytes {
		return ExecutionStats{}, sdkError(op, CodeProtocol, "runtime memory peak is below current usage", nil)
	}
	for name := range result.Metrics {
		if name == "" || len(name) > 256 || strings.IndexFunc(name, func(character rune) bool {
			return unicode.IsControl(character) || unicode.IsSpace(character)
		}) >= 0 {
			return ExecutionStats{}, sdkError(op, CodeProtocol, "runtime metric name is invalid", nil)
		}
	}
	return result, nil
}

func (sandbox *Sandbox) Events(ctx context.Context, request ExecutionEventsRequest) (ExecutionEventBatch, error) {
	const op = "sandbox_events"
	fields, normalized, err := executionEventsFields(request)
	if err != nil {
		return ExecutionEventBatch{}, err
	}
	var result ExecutionEventBatch
	generation, err := sandbox.executionReadRequest(ctx, op, fields, &result)
	if err != nil {
		return ExecutionEventBatch{}, err
	}
	if err := validateExecutionEventBatch(op, sandbox.id, generation, normalized.AfterSequence, result); err != nil {
		return ExecutionEventBatch{}, err
	}
	return result, nil
}

type UpdateResourcesOption interface{ applyUpdateResources(*updateResourcesConfig) }
type updateResourcesOptionFunc func(*updateResourcesConfig)

func (option updateResourcesOptionFunc) applyUpdateResources(config *updateResourcesConfig) {
	option(config)
}

type updateResourcesConfig struct{ operationID string }

func UpdateResourcesOperationID(operationID string) UpdateResourcesOption {
	return updateResourcesOptionFunc(func(config *updateResourcesConfig) { config.operationID = operationID })
}

func (sandbox *Sandbox) UpdateResources(
	ctx context.Context,
	update ExecutionResourceUpdate,
	options ...UpdateResourcesOption,
) error {
	const op = "sandbox_update_resources"
	if sandbox == nil || runtimeIsNil(sandbox.runtime) {
		return invalid(op, "sandbox is not initialized")
	}
	if err := validateExecutionResourceUpdate(op, update); err != nil {
		return err
	}
	config := updateResourcesConfig{}
	for _, option := range options {
		if option != nil {
			option.applyUpdateResources(&config)
		}
	}
	if config.operationID != "" && strings.TrimSpace(config.operationID) == "" {
		return invalid(op, "resource update operation ID cannot be blank")
	}
	if config.operationID == "" {
		operationID, err := randomOperationID("sdk-resource-update-")
		if err != nil {
			return sdkError(op, CodeRuntime, "cannot generate resource update operation ID", err)
		}
		config.operationID = operationID
	}

	sandbox.mu.Lock()
	defer sandbox.mu.Unlock()
	if sandbox.state != StateRunning {
		return invalid(op, "sandbox is not running")
	}
	var info SandboxInfo
	if err := sandbox.lifecycleRequestLocked(ctx, op, map[string]any{
		"operation_id": config.operationID,
		"resources":    update,
	}, &info); err != nil {
		return err
	}
	if err := validateSandboxInfo(op, sandbox.id, sandbox.isolation, info); err != nil {
		return err
	}
	if info.Generation != sandbox.generation {
		return sdkError(op, CodeProtocol, "bridge returned a different execution generation", nil)
	}
	sandbox.updateLocked(info, StateRunning)
	return nil
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

func (sandbox *Sandbox) executionReadRequest(
	ctx context.Context,
	operation string,
	extra map[string]any,
	result any,
) (uint64, error) {
	if sandbox == nil || runtimeIsNil(sandbox.runtime) {
		return 0, invalid(operation, "sandbox is not initialized")
	}
	sandbox.mu.RLock()
	defer sandbox.mu.RUnlock()
	if sandbox.state != StateRunning && sandbox.state != StatePaused {
		return 0, invalid(operation, "sandbox is neither running nor paused")
	}
	fields := map[string]any{
		"sandbox_id": sandbox.id,
		"generation": sandbox.generation,
	}
	for key, value := range extra {
		fields[key] = value
	}
	if err := sandbox.requestLocked(ctx, operation, fields, result); err != nil {
		return 0, err
	}
	return sandbox.generation, nil
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

func randomOperationID(prefix string) (string, error) {
	var value [16]byte
	if _, err := rand.Read(value[:]); err != nil {
		return "", err
	}
	return prefix + hex.EncodeToString(value[:]), nil
}

func executionEventsFields(request ExecutionEventsRequest) (map[string]any, ExecutionEventsRequest, error) {
	if request.Limit == 0 {
		request.Limit = DefaultExecutionEventBatchLimit
	}
	if request.Limit > MaxExecutionEventBatchItems {
		return nil, ExecutionEventsRequest{}, invalid("sandbox_events", "event batch limit must be between 1 and 4096")
	}
	fields := map[string]any{
		"after_sequence": request.AfterSequence,
		"limit":          request.Limit,
	}
	if request.WaitTimeoutMS != nil {
		fields["wait_timeout_ms"] = *request.WaitTimeoutMS
	}
	return fields, request, nil
}

func validateExecutionIdentity(operation, sandboxID string, generation uint64, executionID string, responseGeneration uint64) error {
	if executionID != sandboxID || responseGeneration != generation {
		return sdkError(operation, CodeProtocol, "bridge returned a different execution generation", nil)
	}
	return nil
}

func validExecutionEventKind(kind ExecutionEventKind) bool {
	switch kind {
	case EventContainerCreating, EventContainerCreated, EventContainerStarted, EventContainerStopped,
		EventContainerDeleted, EventContainerPaused, EventContainerResumed, EventResourcesUpdated,
		EventProcessCreated, EventProcessStarted, EventProcessExited, EventOutputDropped, EventRuntimeWarning:
		return true
	default:
		return false
	}
}

func validateExecutionEventBatch(
	operation string,
	sandboxID string,
	generation uint64,
	afterSequence uint64,
	result ExecutionEventBatch,
) error {
	if err := validateExecutionIdentity(operation, sandboxID, generation, result.ExecutionID, result.Generation); err != nil {
		return err
	}
	if result.NextSequence < afterSequence {
		return sdkError(operation, CodeProtocol, "runtime event cursor regressed", nil)
	}
	previous := afterSequence
	for _, event := range result.Events {
		if event.Sequence == 0 || event.Sequence <= previous || event.TimestampUnixNS == 0 || !validExecutionEventKind(event.Kind) {
			return sdkError(operation, CodeProtocol, "runtime event batch is invalid", nil)
		}
		previous = event.Sequence
	}
	if result.NextSequence < previous {
		return sdkError(operation, CodeProtocol, "runtime event cursor precedes the returned batch", nil)
	}
	return nil
}

func validateExecutionResourceUpdate(operation string, update ExecutionResourceUpdate) error {
	if update.MemoryReservation == nil && update.MemorySwap == nil && update.PIDsLimit == nil &&
		update.CPUShares == nil && update.CPUQuota == nil && update.CPUPeriod == nil && update.CPUSetCPUs == nil {
		return invalid(operation, "resource update must change at least one supported field")
	}
	if update.MemorySwap != nil && *update.MemorySwap < -1 {
		return invalid(operation, "memory swap must be -1 or non-negative")
	}
	if update.PIDsLimit != nil && *update.PIDsLimit == 0 {
		return invalid(operation, "PID limit must be greater than zero")
	}
	if update.CPUShares != nil && (*update.CPUShares < 2 || *update.CPUShares > 262_144) {
		return invalid(operation, "CPU shares must be between 2 and 262144")
	}
	if update.CPUQuota != nil && *update.CPUQuota <= 0 {
		return invalid(operation, "CPU quota must be greater than zero")
	}
	if update.CPUPeriod != nil && *update.CPUPeriod == 0 {
		return invalid(operation, "CPU period must be greater than zero")
	}
	if update.CPUSetCPUs != nil && !validCPUSet(*update.CPUSetCPUs) {
		return invalid(operation, "CPU set must be a comma-separated list of indices or ascending ranges")
	}
	return nil
}

func validCPUSet(value string) bool {
	value = strings.TrimSpace(value)
	if value == "" {
		return false
	}
	for _, rawItem := range strings.Split(value, ",") {
		item := strings.TrimSpace(rawItem)
		lowerText, upperText, rangeItem := strings.Cut(item, "-")
		lower, err := strconv.ParseUint(lowerText, 10, 32)
		if err != nil {
			return false
		}
		if !rangeItem {
			continue
		}
		upper, err := strconv.ParseUint(upperText, 10, 32)
		if err != nil || lower > upper {
			return false
		}
	}
	return true
}
