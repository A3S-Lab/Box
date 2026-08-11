package box

import (
	"context"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"errors"
	"fmt"
	"os"
	"strings"
)

// MaxArtifactBytes is the hard ceiling for one in-memory artifact export.
const MaxArtifactBytes uint64 = 8 * 1024 * 1024

type FileOption interface{ applyFile(*fileConfig) }
type fileOptionFunc func(*fileConfig)

func (option fileOptionFunc) applyFile(config *fileConfig) { option(config) }

type fileConfig struct {
	user string
}

func FileAs(user string) FileOption {
	return fileOptionFunc(func(config *fileConfig) { config.user = user })
}

// ArtifactExportOption configures one bounded guest-file export.
type ArtifactExportOption interface{ applyArtifactExport(*artifactExportConfig) }
type artifactExportOptionFunc func(*artifactExportConfig)

func (option artifactExportOptionFunc) applyArtifactExport(config *artifactExportConfig) {
	option(config)
}

type artifactExportConfig struct {
	maxBytes       uint64
	destination    string
	destinationSet bool
	user           string
}

// ArtifactMaxBytes sets the per-export limit, up to MaxArtifactBytes.
func ArtifactMaxBytes(maxBytes uint64) ArtifactExportOption {
	return artifactExportOptionFunc(func(config *artifactExportConfig) { config.maxBytes = maxBytes })
}

// ArtifactTo creates the exact host destination without overwriting it.
func ArtifactTo(destination string) ArtifactExportOption {
	return artifactExportOptionFunc(func(config *artifactExportConfig) {
		config.destination = destination
		config.destinationSet = true
	})
}

// ArtifactAs selects the guest user used for stat and read operations.
func ArtifactAs(user string) ArtifactExportOption {
	return artifactExportOptionFunc(func(config *artifactExportConfig) { config.user = user })
}

type Filesystem struct {
	sandbox *Sandbox
}

func (sandbox *Sandbox) Files() *Filesystem { return &Filesystem{sandbox: sandbox} }

func (filesystem *Filesystem) Write(
	ctx context.Context,
	path string,
	data []byte,
	options ...FileOption,
) (WriteInfo, error) {
	const op = "file_write"
	fields, err := filesystem.fields(op, path, options)
	if err != nil {
		return WriteInfo{}, err
	}
	fields["data_base64"] = base64.StdEncoding.EncodeToString(data)
	var result WriteInfo
	err = filesystem.sandbox.readRequest(ctx, op, fields, &result, true)
	return result, err
}

func (filesystem *Filesystem) WriteString(
	ctx context.Context,
	path, data string,
	options ...FileOption,
) (WriteInfo, error) {
	return filesystem.Write(ctx, path, []byte(data), options...)
}

func (filesystem *Filesystem) Read(
	ctx context.Context,
	path string,
	options ...FileOption,
) ([]byte, error) {
	return filesystem.read(ctx, path, nil, options...)
}

func (filesystem *Filesystem) read(
	ctx context.Context,
	path string,
	maxBytes *uint64,
	options ...FileOption,
) ([]byte, error) {
	const op = "file_read"
	fields, err := filesystem.fields(op, path, options)
	if err != nil {
		return nil, err
	}
	if maxBytes != nil {
		fields["max_bytes"] = *maxBytes
	}
	var result struct {
		Path       string  `json:"path"`
		DataBase64 string  `json:"data_base64"`
		Size       *uint64 `json:"size"`
	}
	if err := filesystem.sandbox.readRequest(ctx, op, fields, &result, true); err != nil {
		return nil, err
	}
	data, err := base64.StdEncoding.DecodeString(result.DataBase64)
	if err != nil {
		return nil, sdkError(op, CodeProtocol, "file contents are not valid base64", err)
	}
	if result.Path != path {
		return nil, sdkError(op, CodeProtocol, "bridge returned file data for a different path", nil)
	}
	if result.Size == nil || *result.Size != uint64(len(data)) {
		return nil, sdkError(op, CodeProtocol, "bridge returned inconsistent file size metadata", nil)
	}
	return data, nil
}

func (filesystem *Filesystem) ReadString(
	ctx context.Context,
	path string,
	options ...FileOption,
) (string, error) {
	data, err := filesystem.Read(ctx, path, options...)
	return string(data), err
}

func (filesystem *Filesystem) Export(
	ctx context.Context,
	path string,
	options ...ArtifactExportOption,
) (Artifact, error) {
	const op = "artifact_export"
	config := artifactExportConfig{maxBytes: MaxArtifactBytes}
	for _, option := range options {
		if option != nil {
			option.applyArtifactExport(&config)
		}
	}
	if config.maxBytes == 0 || config.maxBytes > MaxArtifactBytes {
		return Artifact{}, invalid(op, fmt.Sprintf("max bytes must be between 1 and %d", MaxArtifactBytes))
	}
	if strings.TrimSpace(path) == "" {
		return Artifact{}, invalid(op, "artifact source path cannot be empty")
	}
	if config.destinationSet && strings.TrimSpace(config.destination) == "" {
		return Artifact{}, invalid(op, "artifact destination cannot be blank")
	}
	fileOptions := make([]FileOption, 0, 1)
	if config.user != "" {
		fileOptions = append(fileOptions, FileAs(config.user))
	}
	entry, err := filesystem.Stat(ctx, path, fileOptions...)
	if err != nil {
		return Artifact{}, err
	}
	if entry.Type != "file" {
		return Artifact{}, invalid(op, fmt.Sprintf("artifact source %q must be a file", path))
	}
	if entry.Size > config.maxBytes {
		return Artifact{}, invalid(op, fmt.Sprintf("artifact source is %d bytes; max bytes is %d", entry.Size, config.maxBytes))
	}
	data, err := filesystem.read(ctx, path, &config.maxBytes, fileOptions...)
	if err != nil {
		return Artifact{}, err
	}
	actualSize := uint64(len(data))
	if actualSize > config.maxBytes {
		return Artifact{}, sdkError(op, CodeProtocol, fmt.Sprintf("artifact source grew beyond max bytes (%d) while reading", config.maxBytes), nil)
	}
	if actualSize != entry.Size {
		return Artifact{}, sdkError(op, CodeProtocol, "artifact source changed size while it was being exported", nil)
	}
	if config.destinationSet {
		if err := writeNewArtifactHostFile(op, config.destination, data); err != nil {
			return Artifact{}, err
		}
	}
	digest := sha256.Sum256(data)
	return Artifact{
		Path:     path,
		Data:     data,
		Size:     actualSize,
		SHA256:   hex.EncodeToString(digest[:]),
		HostPath: config.destination,
	}, nil
}

func (filesystem *Filesystem) Stat(
	ctx context.Context,
	path string,
	options ...FileOption,
) (EntryInfo, error) {
	const op = "filesystem_stat"
	fields, err := filesystem.fields(op, path, options)
	if err != nil {
		return EntryInfo{}, err
	}
	var result struct {
		Entry EntryInfo `json:"entry"`
	}
	err = filesystem.sandbox.readRequest(ctx, op, fields, &result, true)
	return result.Entry, err
}

func (filesystem *Filesystem) Exists(
	ctx context.Context,
	path string,
	options ...FileOption,
) (bool, error) {
	_, err := filesystem.Stat(ctx, path, options...)
	if errors.Is(err, ErrNotFound) {
		return false, nil
	}
	return err == nil, err
}

func (filesystem *Filesystem) List(
	ctx context.Context,
	path string,
	depth uint32,
	options ...FileOption,
) ([]EntryInfo, error) {
	const op = "filesystem_list"
	if depth == 0 {
		return nil, invalid(op, "filesystem list depth must be greater than zero")
	}
	fields, err := filesystem.fields(op, path, options)
	if err != nil {
		return nil, err
	}
	fields["depth"] = depth
	var result struct {
		Entries []EntryInfo `json:"entries"`
	}
	if err := filesystem.sandbox.readRequest(ctx, op, fields, &result, true); err != nil {
		return nil, err
	}
	return result.Entries, nil
}

func (filesystem *Filesystem) MakeDir(
	ctx context.Context,
	path string,
	options ...FileOption,
) error {
	const op = "filesystem_make_dir"
	fields, err := filesystem.fields(op, path, options)
	if err != nil {
		return err
	}
	return filesystem.sandbox.readRequest(ctx, op, fields, &struct{}{}, true)
}

func (filesystem *Filesystem) Move(
	ctx context.Context,
	path, destination string,
	options ...FileOption,
) error {
	const op = "filesystem_move"
	if strings.TrimSpace(destination) == "" {
		return invalid(op, "filesystem destination cannot be empty")
	}
	fields, err := filesystem.fields(op, path, options)
	if err != nil {
		return err
	}
	fields["destination"] = destination
	return filesystem.sandbox.readRequest(ctx, op, fields, &struct{}{}, true)
}

func (filesystem *Filesystem) Remove(
	ctx context.Context,
	path string,
	options ...FileOption,
) error {
	const op = "filesystem_remove"
	fields, err := filesystem.fields(op, path, options)
	if err != nil {
		return err
	}
	return filesystem.sandbox.readRequest(ctx, op, fields, &struct{}{}, true)
}

func (filesystem *Filesystem) fields(
	operation, path string,
	options []FileOption,
) (map[string]any, error) {
	if filesystem == nil || filesystem.sandbox == nil {
		return nil, invalid(operation, "filesystem handle is not initialized")
	}
	if strings.TrimSpace(path) == "" {
		return nil, invalid(operation, "filesystem path cannot be empty")
	}
	config := fileConfig{}
	for _, option := range options {
		if option != nil {
			option.applyFile(&config)
		}
	}
	if config.user != "" && strings.TrimSpace(config.user) == "" {
		return nil, invalid(operation, "filesystem user cannot be blank")
	}
	fields := map[string]any{"path": path}
	if config.user != "" {
		fields["user"] = config.user
	}
	return fields, nil
}

func writeNewArtifactHostFile(operation, destination string, data []byte) error {
	file, err := os.OpenFile(destination, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if err != nil {
		return sdkError(operation, CodeRuntime, fmt.Sprintf("could not create artifact destination %q", destination), err)
	}

	var writeErr error
	for offset := 0; offset < len(data) && writeErr == nil; {
		written, err := file.Write(data[offset:])
		offset += written
		switch {
		case err != nil:
			writeErr = err
		case written == 0:
			writeErr = errors.New("artifact destination write made no progress")
		}
	}
	if writeErr == nil {
		writeErr = file.Sync()
	}
	if closeErr := file.Close(); writeErr == nil {
		writeErr = closeErr
	}
	if writeErr == nil {
		return nil
	}

	message := fmt.Sprintf("could not write artifact destination %q", destination)
	if cleanupErr := os.Remove(destination); cleanupErr != nil {
		message = fmt.Sprintf("%s; partial-file cleanup failed: %v", message, cleanupErr)
	}
	return sdkError(operation, CodeRuntime, message, writeErr)
}
