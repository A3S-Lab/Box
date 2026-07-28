package box

import (
	"context"
	"strings"
)

type ImageBuilder struct {
	client     *Client
	contextDir string
	dockerfile string
	tag        string
	buildArgs  map[string]string
	platforms  []string
	target     string
	noCache    bool
}

func (client *Client) Image(contextDir string) *ImageBuilder {
	return &ImageBuilder{client: client, contextDir: contextDir, buildArgs: make(map[string]string)}
}

func (builder *ImageBuilder) Dockerfile(path string) *ImageBuilder {
	builder.dockerfile = path
	return builder
}

func (builder *ImageBuilder) Tag(reference string) *ImageBuilder {
	builder.tag = reference
	return builder
}

func (builder *ImageBuilder) BuildArg(key, value string) *ImageBuilder {
	builder.buildArgs[key] = value
	return builder
}

func (builder *ImageBuilder) Platform(platform string) *ImageBuilder {
	builder.platforms = append(builder.platforms, platform)
	return builder
}

func (builder *ImageBuilder) Target(target string) *ImageBuilder {
	builder.target = target
	return builder
}

func (builder *ImageBuilder) NoCache(enabled bool) *ImageBuilder {
	builder.noCache = enabled
	return builder
}

func (builder *ImageBuilder) Build(ctx context.Context) (BuildImageInfo, error) {
	const op = "image_build"
	if builder == nil || builder.client == nil {
		return BuildImageInfo{}, invalid(op, "image builder is not initialized")
	}
	if strings.TrimSpace(builder.contextDir) == "" {
		return BuildImageInfo{}, invalid(op, "build context directory cannot be empty")
	}
	if builder.dockerfile != "" && strings.TrimSpace(builder.dockerfile) == "" {
		return BuildImageInfo{}, invalid(op, "Dockerfile path cannot be blank")
	}
	if builder.tag != "" && strings.TrimSpace(builder.tag) == "" {
		return BuildImageInfo{}, invalid(op, "image tag cannot be blank")
	}
	if builder.target != "" && strings.TrimSpace(builder.target) == "" {
		return BuildImageInfo{}, invalid(op, "build target cannot be blank")
	}
	for key := range builder.buildArgs {
		if strings.TrimSpace(key) == "" {
			return BuildImageInfo{}, invalid(op, "build argument name cannot be empty")
		}
	}
	for _, platform := range builder.platforms {
		if strings.TrimSpace(platform) == "" {
			return BuildImageInfo{}, invalid(op, "platform cannot be empty")
		}
	}
	fields := map[string]any{
		"context_dir": builder.contextDir,
		"build_args":  cloneStringMap(builder.buildArgs),
		"quiet":       true,
		"platforms":   append([]string{}, builder.platforms...),
		"no_cache":    builder.noCache,
	}
	if builder.dockerfile != "" {
		fields["dockerfile"] = builder.dockerfile
	}
	if builder.tag != "" {
		fields["tag"] = builder.tag
	}
	if builder.target != "" {
		fields["target"] = builder.target
	}
	var result BuildImageInfo
	err := builder.client.request(ctx, op, fields, &result)
	return result, err
}
