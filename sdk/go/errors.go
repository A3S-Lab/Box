package box

import (
	"context"
	"errors"
	"fmt"
)

// ErrorCode is a stable SDK or runtime error category.
type ErrorCode string

const (
	CodeInvalidRequest ErrorCode = "invalid_request"
	CodeNotFound       ErrorCode = "not_found"
	CodeConflict       ErrorCode = "conflict"
	CodeUnavailable    ErrorCode = "unavailable"
	CodeRuntime        ErrorCode = "runtime_error"
	CodeProtocol       ErrorCode = "bridge_protocol_error"
	CodeBinaryNotFound ErrorCode = "binary_not_found"
	// CodeNotInstalled is retained as a source-compatible alias.
	// Deprecated: use CodeBinaryNotFound.
	CodeNotInstalled     ErrorCode = CodeBinaryNotFound
	CodeCanceled         ErrorCode = "canceled"
	CodeDeadlineExceeded ErrorCode = "deadline_exceeded"
	CodeBridgeTimeout    ErrorCode = "bridge_timeout"
)

var (
	ErrInvalidRequest = &Error{Code: CodeInvalidRequest}
	ErrNotFound       = &Error{Code: CodeNotFound}
	ErrConflict       = &Error{Code: CodeConflict}
	ErrUnavailable    = &Error{Code: CodeUnavailable}
	ErrRuntime        = &Error{Code: CodeRuntime}
	ErrProtocol       = &Error{Code: CodeProtocol}
	ErrBinaryNotFound = &Error{Code: CodeBinaryNotFound}
	// ErrNotInstalled is retained as a source-compatible alias.
	// Deprecated: use ErrBinaryNotFound.
	ErrNotInstalled     = ErrBinaryNotFound
	ErrCanceled         = &Error{Code: CodeCanceled}
	ErrDeadlineExceeded = &Error{Code: CodeDeadlineExceeded}
	ErrBridgeTimeout    = &Error{Code: CodeBridgeTimeout}
)

// Error is returned for validation, transport, protocol, and runtime failures.
// Cause is retained so errors.Is continues to recognize context cancellation
// and process errors.
type Error struct {
	Op      string
	Code    ErrorCode
	Message string
	Cause   error
}

func (e *Error) Error() string {
	if e == nil {
		return "<nil>"
	}
	message := e.Message
	if message == "" {
		message = string(e.Code)
	}
	if e.Op == "" {
		return message
	}
	return fmt.Sprintf("a3s-box %s: %s", e.Op, message)
}

func (e *Error) Unwrap() error {
	if e == nil {
		return nil
	}
	return e.Cause
}

func (e *Error) Is(target error) bool {
	other, ok := target.(*Error)
	if !ok || e == nil || other == nil {
		return false
	}
	return other.Code != "" && e.Code == other.Code
}

func sdkError(op string, code ErrorCode, message string, cause error) error {
	return &Error{Op: op, Code: code, Message: message, Cause: cause}
}

func invalid(op, message string) error {
	return sdkError(op, CodeInvalidRequest, message, nil)
}

func contextError(op string, err error) error {
	switch {
	case errors.Is(err, context.Canceled):
		return sdkError(op, CodeCanceled, "request canceled", err)
	case errors.Is(err, context.DeadlineExceeded):
		return sdkError(op, CodeDeadlineExceeded, "request deadline exceeded", err)
	default:
		return sdkError(op, CodeRuntime, err.Error(), err)
	}
}
