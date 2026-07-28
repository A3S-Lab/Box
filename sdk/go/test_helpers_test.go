package box

import (
	"context"
	"encoding/json"
	"fmt"
	"sync"

	"github.com/A3S-Lab/Box/sdk/go/v3/internal/bridge"
)

type fakeRuntime struct {
	mu       sync.Mutex
	requests []map[string]any
	handler  func(context.Context, map[string]any) (any, error)
}

func (runtime *fakeRuntime) Request(ctx context.Context, request any, result any) error {
	payload, err := json.Marshal(request)
	if err != nil {
		return err
	}
	var decoded map[string]any
	if err := json.Unmarshal(payload, &decoded); err != nil {
		return err
	}
	runtime.mu.Lock()
	runtime.requests = append(runtime.requests, decoded)
	runtime.mu.Unlock()

	var value any = map[string]any{}
	if decoded["operation"] == "sdk_capabilities" {
		value = fullCapabilities()
	} else if runtime.handler != nil {
		value, err = runtime.handler(ctx, decoded)
		if err != nil {
			return err
		}
	}
	if result == nil {
		return nil
	}
	encoded, err := json.Marshal(value)
	if err != nil {
		return err
	}
	return json.Unmarshal(encoded, result)
}

func (runtime *fakeRuntime) Requests() []map[string]any {
	runtime.mu.Lock()
	defer runtime.mu.Unlock()
	result := make([]map[string]any, len(runtime.requests))
	copy(result, runtime.requests)
	return result
}

func (runtime *fakeRuntime) Operations() []string {
	requests := runtime.Requests()
	operations := make([]string, 0, len(requests))
	for _, request := range requests {
		operations = append(operations, fmt.Sprint(request["operation"]))
	}
	return operations
}

func fullCapabilities() Capabilities {
	return Capabilities{
		ProtocolVersion: bridge.ProtocolVersion,
		Operations:      append([]string(nil), bridge.RequiredOperations...),
	}
}

func mustClient(runtime Runtime) *Client {
	client, err := NewClient(context.Background(), WithRuntime(runtime))
	if err != nil {
		panic(err)
	}
	return client
}
