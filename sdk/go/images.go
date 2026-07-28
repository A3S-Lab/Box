package box

import (
	"context"
	"strings"
)

type PullOption interface{ applyPull(*pullConfig) }
type pullOptionFunc func(*pullConfig)

func (option pullOptionFunc) applyPull(config *pullConfig) { option(config) }

type pullConfig struct {
	force           bool
	platform        string
	credentials     *RegistryCredentials
	signaturePolicy *SignaturePolicy
}

func PullForce() PullOption {
	return pullOptionFunc(func(config *pullConfig) { config.force = true })
}

func PullPlatform(platform string) PullOption {
	return pullOptionFunc(func(config *pullConfig) { config.platform = platform })
}

func PullCredentials(credentials RegistryCredentials) PullOption {
	return pullOptionFunc(func(config *pullConfig) { config.credentials = &credentials })
}

func PullSignaturePolicy(policy SignaturePolicy) PullOption {
	return pullOptionFunc(func(config *pullConfig) { config.signaturePolicy = &policy })
}

func (client *Client) PullImage(ctx context.Context, reference string, options ...PullOption) (ImageInfo, error) {
	const op = "image_pull"
	if strings.TrimSpace(reference) == "" {
		return ImageInfo{}, invalid(op, "image reference cannot be empty")
	}
	config := pullConfig{}
	for _, option := range options {
		if option != nil {
			option.applyPull(&config)
		}
	}
	fields := map[string]any{"reference": reference, "force": config.force}
	if config.platform != "" {
		if strings.TrimSpace(config.platform) == "" {
			return ImageInfo{}, invalid(op, "image platform cannot be blank")
		}
		fields["platform"] = config.platform
	}
	if config.credentials != nil {
		if err := config.credentials.validate(); err != nil {
			return ImageInfo{}, err
		}
		fields["credentials"] = config.credentials.bridgeValue()
	}
	if config.signaturePolicy != nil {
		if err := config.signaturePolicy.validate(); err != nil {
			return ImageInfo{}, err
		}
		fields["signature_policy"] = config.signaturePolicy.bridgeValue()
	}
	var image ImageInfo
	if err := client.request(ctx, op, fields, &image); err != nil {
		return ImageInfo{}, err
	}
	return image, nil
}

func (client *Client) GetImage(ctx context.Context, reference string) (*ImageInfo, error) {
	const op = "image_get"
	if strings.TrimSpace(reference) == "" {
		return nil, invalid(op, "image reference cannot be empty")
	}
	var result struct {
		Image *ImageInfo `json:"image"`
	}
	if err := client.request(ctx, op, map[string]any{"reference": reference}, &result); err != nil {
		return nil, err
	}
	return result.Image, nil
}

func (client *Client) ListImages(ctx context.Context) ([]ImageInfo, error) {
	var result struct {
		Images []ImageInfo `json:"images"`
	}
	if err := client.request(ctx, "image_list", nil, &result); err != nil {
		return nil, err
	}
	return result.Images, nil
}

func (client *Client) InspectImage(ctx context.Context, reference string) (*ImageInspectInfo, error) {
	const op = "image_inspect"
	if strings.TrimSpace(reference) == "" {
		return nil, invalid(op, "image reference cannot be empty")
	}
	var result struct {
		Image *ImageInspectInfo `json:"image"`
	}
	if err := client.request(ctx, op, map[string]any{"reference": reference}, &result); err != nil {
		return nil, err
	}
	return result.Image, nil
}

func (client *Client) ImageHistory(ctx context.Context, reference string) ([]ImageHistoryInfo, error) {
	const op = "image_history"
	if strings.TrimSpace(reference) == "" {
		return nil, invalid(op, "image reference cannot be empty")
	}
	var result struct {
		History []ImageHistoryInfo `json:"history"`
	}
	if err := client.request(ctx, op, map[string]any{"reference": reference}, &result); err != nil {
		return nil, err
	}
	return result.History, nil
}

func (client *Client) TagImage(ctx context.Context, source, target string) (ImageInfo, error) {
	const op = "image_tag"
	if strings.TrimSpace(source) == "" || strings.TrimSpace(target) == "" {
		return ImageInfo{}, invalid(op, "source and target image references cannot be empty")
	}
	var image ImageInfo
	err := client.request(ctx, op, map[string]any{"source": source, "target": target}, &image)
	return image, err
}

type PushOption interface{ applyPush(*pushConfig) }
type pushOptionFunc func(*pushConfig)

func (option pushOptionFunc) applyPush(config *pushConfig) { option(config) }

type pushConfig struct {
	credentials *RegistryCredentials
	protocol    RegistryProtocol
}

func PushCredentials(credentials RegistryCredentials) PushOption {
	return pushOptionFunc(func(config *pushConfig) { config.credentials = &credentials })
}

func PushProtocol(protocol RegistryProtocol) PushOption {
	return pushOptionFunc(func(config *pushConfig) { config.protocol = protocol })
}

func (client *Client) PushImage(
	ctx context.Context,
	source, target string,
	options ...PushOption,
) (PushImageInfo, error) {
	const op = "image_push"
	if strings.TrimSpace(source) == "" || strings.TrimSpace(target) == "" {
		return PushImageInfo{}, invalid(op, "source and target image references cannot be empty")
	}
	config := pushConfig{}
	for _, option := range options {
		if option != nil {
			option.applyPush(&config)
		}
	}
	fields := map[string]any{"source": source, "target": target}
	if config.credentials != nil {
		if err := config.credentials.validate(); err != nil {
			return PushImageInfo{}, err
		}
		fields["credentials"] = config.credentials.bridgeValue()
	}
	if config.protocol != "" {
		if config.protocol != RegistryHTTP && config.protocol != RegistryHTTPS {
			return PushImageInfo{}, invalid(op, "registry protocol must be http or https")
		}
		fields["registry_protocol"] = config.protocol
	}
	var result PushImageInfo
	err := client.request(ctx, op, fields, &result)
	return result, err
}

func (client *Client) RemoveImage(ctx context.Context, reference string) error {
	const op = "image_remove"
	if strings.TrimSpace(reference) == "" {
		return invalid(op, "image reference cannot be empty")
	}
	return client.request(ctx, op, map[string]any{"reference": reference}, &struct{}{})
}

func (client *Client) EvictImages(ctx context.Context) ([]string, error) {
	var result struct {
		References []string `json:"references"`
	}
	if err := client.request(ctx, "image_evict", nil, &result); err != nil {
		return nil, err
	}
	return result.References, nil
}
