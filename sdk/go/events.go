package box

import (
	"context"
	"io"
	"sync"
	"time"
)

const (
	DefaultExecutionEventStreamBatchLimit = DefaultExecutionEventBatchLimit
	DefaultExecutionEventStreamWait       = time.Second
)

// ExecutionEventStreamOptions configures one exact-generation event stream.
// Zero values select the bounded defaults.
type ExecutionEventStreamOptions struct {
	AfterSequence uint64
	BatchLimit    uint32
	WaitTimeout   time.Duration
}

// ExecutionEventStream incrementally consumes ordered events with one event of
// caller-driven backpressure at a time. It never follows a restart generation.
type ExecutionEventStream struct {
	sandbox       *Sandbox
	runtime       Runtime
	sandboxID     string
	generation    uint64
	batchLimit    uint32
	waitTimeoutMS uint64

	nextMu     sync.Mutex
	mu         sync.Mutex
	cursor     uint64
	checkpoint uint64
	pending    []ExecutionRuntimeEvent
	closed     bool
	ctx        context.Context
	cancel     context.CancelFunc
}

// StreamEvents creates a continuous, cancellable stream for the currently
// visible running or paused Sandbox generation.
func (sandbox *Sandbox) StreamEvents(options ExecutionEventStreamOptions) (*ExecutionEventStream, error) {
	const op = "sandbox_event_stream"
	if sandbox == nil || runtimeIsNil(sandbox.runtime) {
		return nil, invalid(op, "sandbox is not initialized")
	}
	batchLimit := options.BatchLimit
	if batchLimit == 0 {
		batchLimit = DefaultExecutionEventStreamBatchLimit
	}
	if batchLimit > MaxExecutionEventBatchItems {
		return nil, invalid(op, "event stream batch limit must be between 1 and 4096")
	}
	waitTimeout := options.WaitTimeout
	if waitTimeout == 0 {
		waitTimeout = DefaultExecutionEventStreamWait
	}
	if waitTimeout < time.Millisecond || waitTimeout%time.Millisecond != 0 {
		return nil, invalid(op, "event stream wait timeout must be a positive whole number of milliseconds")
	}

	sandbox.mu.RLock()
	defer sandbox.mu.RUnlock()
	if sandbox.state != StateRunning && sandbox.state != StatePaused {
		return nil, invalid(op, "sandbox is neither running nor paused")
	}
	streamContext, cancel := context.WithCancel(context.Background())
	return &ExecutionEventStream{
		sandbox:       sandbox,
		runtime:       sandbox.runtime,
		sandboxID:     sandbox.id,
		generation:    sandbox.generation,
		batchLimit:    batchLimit,
		waitTimeoutMS: uint64(waitTimeout / time.Millisecond),
		cursor:        options.AfterSequence,
		checkpoint:    options.AfterSequence,
		ctx:           streamContext,
		cancel:        cancel,
	}, nil
}

// Next waits for and returns the next ordered event. Context cancellation or
// Close interrupts an active long poll and releases its bridge process.
func (stream *ExecutionEventStream) Next(ctx context.Context) (ExecutionRuntimeEvent, error) {
	const op = "sandbox_event_stream"
	if stream == nil || stream.sandbox == nil || runtimeIsNil(stream.runtime) {
		return ExecutionRuntimeEvent{}, invalid(op, "event stream is not initialized")
	}
	if ctx == nil {
		return ExecutionRuntimeEvent{}, invalid(op, "context cannot be nil")
	}

	stream.nextMu.Lock()
	defer stream.nextMu.Unlock()
	for {
		stream.mu.Lock()
		if stream.closed {
			stream.mu.Unlock()
			return ExecutionRuntimeEvent{}, io.EOF
		}
		if len(stream.pending) != 0 {
			event := stream.pending[0]
			stream.pending = stream.pending[1:]
			stream.checkpoint = event.Sequence
			if len(stream.pending) == 0 {
				stream.checkpoint = stream.cursor
			}
			stream.mu.Unlock()
			return event, nil
		}
		cursor := stream.cursor
		stream.mu.Unlock()

		if err := stream.requireGeneration(op); err != nil {
			stream.fail()
			return ExecutionRuntimeEvent{}, err
		}
		requestContext, cancelRequest := context.WithCancel(ctx)
		stopCancellation := context.AfterFunc(stream.ctx, cancelRequest)
		var result ExecutionEventBatch
		err := stream.runtime.Request(requestContext, map[string]any{
			"operation":       "sandbox_events",
			"sandbox_id":      stream.sandboxID,
			"generation":      stream.generation,
			"after_sequence":  cursor,
			"limit":           stream.batchLimit,
			"wait_timeout_ms": stream.waitTimeoutMS,
		}, &result)
		stopCancellation()
		cancelRequest()
		if stream.isClosed() {
			return ExecutionRuntimeEvent{}, io.EOF
		}
		if err != nil {
			stream.fail()
			return ExecutionRuntimeEvent{}, err
		}
		if err := stream.requireGeneration(op); err != nil {
			stream.fail()
			return ExecutionRuntimeEvent{}, err
		}
		if err := validateExecutionEventBatch(op, stream.sandboxID, stream.generation, cursor, result); err != nil {
			stream.fail()
			return ExecutionRuntimeEvent{}, err
		}

		stream.mu.Lock()
		stream.cursor = result.NextSequence
		stream.pending = append(stream.pending[:0], result.Events...)
		if len(stream.pending) == 0 {
			stream.checkpoint = stream.cursor
		}
		stream.mu.Unlock()
	}
}

// Cursor returns a safe resume cursor for every event already delivered by the
// stream. It never advances past buffered, undelivered events.
func (stream *ExecutionEventStream) Cursor() uint64 {
	if stream == nil {
		return 0
	}
	stream.mu.Lock()
	defer stream.mu.Unlock()
	return stream.checkpoint
}

// Close idempotently ends the stream and cancels an active long poll.
func (stream *ExecutionEventStream) Close() error {
	if stream == nil {
		return nil
	}
	stream.mu.Lock()
	if !stream.closed {
		stream.closed = true
		stream.pending = nil
		stream.cancel()
	}
	stream.mu.Unlock()
	return nil
}

func (stream *ExecutionEventStream) requireGeneration(operation string) error {
	stream.sandbox.mu.RLock()
	defer stream.sandbox.mu.RUnlock()
	if stream.sandbox.generation != stream.generation {
		return sdkError(operation, CodeConflict, "sandbox generation changed while streaming events", nil)
	}
	if stream.sandbox.state != StateRunning && stream.sandbox.state != StatePaused {
		return sdkError(operation, CodeConflict, "sandbox is no longer observable", nil)
	}
	return nil
}

func (stream *ExecutionEventStream) isClosed() bool {
	stream.mu.Lock()
	defer stream.mu.Unlock()
	return stream.closed
}

func (stream *ExecutionEventStream) fail() {
	stream.mu.Lock()
	if !stream.closed {
		stream.closed = true
		stream.pending = nil
		stream.cancel()
	}
	stream.mu.Unlock()
}
