package box

import (
	"context"
	"fmt"
	"reflect"
	"sort"
	"strings"

	"github.com/A3S-Lab/Box/sdk/go/v3/internal/bridge"
)

// Client is a concurrency-safe entry point for local A3S Box resources.
type Client struct {
	runtime      Runtime
	capabilities Capabilities
}

type ClientOption interface {
	applyClient(*clientConfig)
}

type clientOptionFunc func(*clientConfig)

func (option clientOptionFunc) applyClient(config *clientConfig) { option(config) }

type clientConfig struct {
	runtime Runtime
}

// WithRuntime injects a typed runtime implementation. It is useful for tests
// and custom local transports; normal applications can use NewClient directly.
func WithRuntime(runtime Runtime) ClientOption {
	return clientOptionFunc(func(config *clientConfig) {
		config.runtime = runtime
	})
}

// NewClient verifies the full machine bridge capability inventory before the
// client can issue a mutating request.
func NewClient(ctx context.Context, options ...ClientOption) (*Client, error) {
	config := clientConfig{runtime: NewLocalRuntime()}
	for _, option := range options {
		if option != nil {
			option.applyClient(&config)
		}
	}
	if runtimeIsNil(config.runtime) {
		return nil, invalid("new_client", "runtime cannot be nil")
	}
	client := &Client{runtime: config.runtime}
	var capabilities Capabilities
	if err := client.request(ctx, "sdk_capabilities", nil, &capabilities); err != nil {
		return nil, err
	}
	if capabilities.ProtocolVersion != bridge.ProtocolVersion {
		return nil, sdkError(
			"sdk_capabilities",
			CodeProtocol,
			fmt.Sprintf("runtime capability protocol version %d is unsupported", capabilities.ProtocolVersion),
			nil,
		)
	}
	available := make(map[string]struct{}, len(capabilities.Operations))
	for _, operation := range capabilities.Operations {
		available[operation] = struct{}{}
	}
	missing := make([]string, 0)
	for _, operation := range bridge.RequiredOperations {
		if _, ok := available[operation]; !ok {
			missing = append(missing, operation)
		}
	}
	if len(missing) != 0 {
		sort.Strings(missing)
		return nil, sdkError(
			"sdk_capabilities",
			CodeUnavailable,
			"installed A3S Box runtime is missing required operations: "+strings.Join(missing, ", "),
			nil,
		)
	}
	capabilities.Operations = append([]string(nil), capabilities.Operations...)
	client.capabilities = capabilities
	return client, nil
}

// Create is the shortest local Sandbox entry point.
func Create(ctx context.Context, image string, options ...ClientOption) (*Sandbox, error) {
	client, err := NewClient(ctx, options...)
	if err != nil {
		return nil, err
	}
	return client.Sandbox(image).Start(ctx)
}

// Connect attaches to an existing local Sandbox by ID.
func Connect(ctx context.Context, sandboxID string, options ...ClientOption) (*Sandbox, error) {
	client, err := NewClient(ctx, options...)
	if err != nil {
		return nil, err
	}
	return client.ConnectSandbox(ctx, sandboxID)
}

func (client *Client) Capabilities() Capabilities {
	if client == nil {
		return Capabilities{}
	}
	capabilities := client.capabilities
	capabilities.Operations = append([]string(nil), capabilities.Operations...)
	return capabilities
}

func SupportedOperations() []string {
	return append([]string(nil), bridge.RequiredOperations...)
}

func (client *Client) request(
	ctx context.Context,
	operation string,
	fields map[string]any,
	result any,
) error {
	if client == nil || runtimeIsNil(client.runtime) {
		return invalid(operation, "client is not initialized")
	}
	if ctx == nil {
		return invalid(operation, "context cannot be nil")
	}
	request := make(map[string]any, len(fields)+1)
	request["operation"] = operation
	for key, value := range fields {
		request[key] = value
	}
	return client.runtime.Request(ctx, request, result)
}

func runtimeIsNil(runtime Runtime) bool {
	if runtime == nil {
		return true
	}
	value := reflect.ValueOf(runtime)
	switch value.Kind() {
	case reflect.Chan, reflect.Func, reflect.Interface, reflect.Map, reflect.Pointer, reflect.Slice:
		return value.IsNil()
	default:
		return false
	}
}
