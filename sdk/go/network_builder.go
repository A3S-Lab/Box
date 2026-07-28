package box

import (
	"context"
	"net"
	"strings"
)

type NetworkBuilder struct {
	client *Client
	name   string
	subnet string
	labels map[string]string
}

func (client *Client) Network(name string) *NetworkBuilder {
	return &NetworkBuilder{
		client: client,
		name:   name,
		subnet: "10.89.0.0/24",
		labels: make(map[string]string),
	}
}

func (builder *NetworkBuilder) Subnet(subnet string) *NetworkBuilder {
	builder.subnet = subnet
	return builder
}

func (builder *NetworkBuilder) Label(key, value string) *NetworkBuilder {
	builder.labels[key] = value
	return builder
}

func (builder *NetworkBuilder) Create(ctx context.Context) (NetworkInfo, error) {
	const op = "network_create"
	if builder == nil || builder.client == nil {
		return NetworkInfo{}, invalid(op, "network builder is not initialized")
	}
	if strings.TrimSpace(builder.name) == "" {
		return NetworkInfo{}, invalid(op, "network name cannot be empty")
	}
	if _, _, err := net.ParseCIDR(builder.subnet); err != nil {
		return NetworkInfo{}, invalid(op, "network subnet must be valid CIDR notation")
	}
	if err := validateLabels(op, builder.labels); err != nil {
		return NetworkInfo{}, err
	}
	var result NetworkInfo
	err := builder.client.request(ctx, op, map[string]any{
		"name":   builder.name,
		"subnet": builder.subnet,
		"labels": cloneStringMap(builder.labels),
	}, &result)
	return result, err
}
