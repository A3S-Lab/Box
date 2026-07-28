package box

import (
	"context"
	"strings"
)

type VolumeBuilder struct {
	client    *Client
	name      string
	labels    map[string]string
	sizeLimit uint64
}

func (client *Client) Volume(name string) *VolumeBuilder {
	return &VolumeBuilder{client: client, name: name, labels: make(map[string]string)}
}

func (builder *VolumeBuilder) Label(key, value string) *VolumeBuilder {
	builder.labels[key] = value
	return builder
}

func (builder *VolumeBuilder) SizeLimit(bytes uint64) *VolumeBuilder {
	builder.sizeLimit = bytes
	return builder
}

func (builder *VolumeBuilder) Create(ctx context.Context) (VolumeInfo, error) {
	const op = "volume_create"
	if builder == nil || builder.client == nil {
		return VolumeInfo{}, invalid(op, "volume builder is not initialized")
	}
	if strings.TrimSpace(builder.name) == "" {
		return VolumeInfo{}, invalid(op, "volume name cannot be empty")
	}
	if err := validateLabels(op, builder.labels); err != nil {
		return VolumeInfo{}, err
	}
	var result VolumeInfo
	err := builder.client.request(ctx, op, map[string]any{
		"name":       builder.name,
		"labels":     cloneStringMap(builder.labels),
		"size_limit": builder.sizeLimit,
	}, &result)
	return result, err
}
